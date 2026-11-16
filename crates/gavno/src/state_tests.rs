//! In-process state tests: no display backend, renderer, or public Wayland socket.
use super::*;
use std::{os::unix::net::UnixStream, time::Duration};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    shm: Option<wl_shm::WlShm>,
    pointer_positions: Vec<(f64, f64)>,
    pointer_buttons: Vec<u32>,
    keyboard_keys: Vec<u32>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()))
                }
                "wl_seat" => {
                    let seat: wl_seat::WlSeat = registry.bind(name, version.min(8), qh, ());
                    seat.get_pointer(qh, ());
                    seat.get_keyboard(qh, ());
                }
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "xdg_wm_base" => state.shell = Some(registry.bind(name, version.min(6), qh, ())),
                _ => {}
            }
        }
    }
}
wayland_client::delegate_noop!(Client: ignore wl_seat::WlSeat);
impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.pointer_positions.push((surface_x, surface_y)),
            wl_pointer::Event::Button { button, .. } => state.pointer_buttons.push(button),
            _ => {}
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for Client {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_keyboard::Event::Key { key, .. } = event { state.keyboard_keys.push(key); }
    }
}
wayland_client::delegate_noop!(Client: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Client: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(Client: ignore xdg_wm_base::XdgWmBase);
wayland_client::delegate_noop!(Client: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(Client: ignore wl_buffer::WlBuffer);
impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(
        _: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
        }
    }
}
wayland_client::delegate_noop!(Client: ignore xdg_toplevel::XdgToplevel);

pub(crate) struct Harness {
    pub gv: Gavno,
    pub(crate) event_loop: EventLoop<'static, Gavno>,
    connection: Connection,
    queue: EventQueue<Client>,
    client: Client,
    surfaces: Vec<(Window, wl_surface::WlSurface)>,
}
impl Harness {
    pub fn new() -> Self {
        let mut event_loop = EventLoop::try_new().unwrap();
        let display = Display::new().unwrap();
        let mut gv = Gavno::new(
            &mut event_loop,
            display,
            Config::build(true, None, |_| None).0,
        );
        let (server_socket, client_socket) = UnixStream::pair().unwrap();
        client_socket.set_nonblocking(true).unwrap();
        gv.display_handle
            .insert_client(server_socket, Arc::new(ClientState::default()))
            .unwrap();
        let connection = Connection::from_socket(client_socket).unwrap();
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut h = Self {
            gv,
            event_loop,
            connection,
            queue,
            client: Client::default(),
            surfaces: Vec::new(),
        };
        h.exchange();
        h.exchange();
        h
    }

    fn exchange(&mut self) {
        self.connection.flush().unwrap();
        self.event_loop
            .dispatch(Some(Duration::ZERO), &mut self.gv)
            .unwrap();
        self.gv.display_handle.flush_clients().unwrap();
        self.queue.dispatch_pending(&mut self.client).unwrap();
        if let Some(read) = self.queue.prepare_read() {
            let _ = read.read();
        }
        self.queue.dispatch_pending(&mut self.client).unwrap();
    }

    pub fn window(&mut self) -> Window {
        let qh = self.queue.handle();
        let surface = self
            .client
            .compositor
            .as_ref()
            .unwrap()
            .create_surface(&qh, ());
        let shell_surface = self
            .client
            .shell
            .as_ref()
            .unwrap()
            .get_xdg_surface(&surface, &qh, ());
        let _toplevel = shell_surface.get_toplevel(&qh, ());
        self.exchange();
        let window = self.gv.unmapped.pop().unwrap();
        self.surfaces.push((window.clone(), surface));
        window
    }
    pub(crate) fn keyboard_keys(&mut self) -> Vec<u32> {
        self.exchange(); self.exchange();
        std::mem::take(&mut self.client.keyboard_keys)
    }
    pub(crate) fn pointer_buttons(&mut self) -> Vec<u32> {
        self.exchange(); self.exchange();
        std::mem::take(&mut self.client.pointer_buttons)
    }
    pub fn pointer_positions(&mut self) -> Vec<(f64, f64)> {
        self.exchange();
        self.exchange();
        std::mem::take(&mut self.client.pointer_positions)
    }
    /// Commit a real shm buffer and acknowledge the latest compositor configure.
    pub fn commit_size(&mut self, window: &Window, width: i32, height: i32) {
        use std::os::fd::AsFd;
        self.exchange();
        self.exchange();
        let path = std::env::temp_dir().join(format!(
            "gavno-test-{}-{:?}",
            std::process::id(),
            smithay::utils::SERIAL_COUNTER.next_serial()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        let length = width * height * 4;
        file.set_len(length as u64).unwrap();
        let qh = self.queue.handle();
        let pool = self
            .client
            .shm
            .as_ref()
            .unwrap()
            .create_pool(file.as_fd(), length, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Xrgb8888,
            &qh,
            (),
        );
        let surface = &self.surfaces.iter().find(|(w, _)| w == window).unwrap().1;
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        pool.destroy();
        self.exchange();
        self.exchange();
        assert_eq!(window.geometry().size, (width, height).into());
    }
}

#[test]
fn locked_sessions_do_not_schedule_hidden_window_animations() {
    let mut h = Harness::new();
    let window = h.window();
    h.gv.map_new_window(window);
    assert!(h.gv.is_animating());
    h.gv.protocols.locks.locked = true;
    assert!(!h.gv.is_animating());
    let window = h.window();
    h.gv.map_new_window(window);
    assert!(h.gv.window_animations.is_animating());
    assert!(!h.gv.is_animating());
    h.gv.protocols.locks.locked = false;
    assert!(h.gv.is_animating());
}

#[test]
fn locked_sessions_do_not_schedule_hidden_overlay_animations() {
    let mut h = Harness::new();
    let output = Output::new(
        "test".into(),
        smithay::output::PhysicalProperties {
            size: (0, 0).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
        },
    );
    let from = Rectangle::new((0, 0).into(), (100, 100).into());
    let to = Rectangle::new((0, 0).into(), (200, 200).into());
    h.gv.overlay
        .preview(output, crate::snap::Zone::Left, from, to, Duration::ZERO);
    assert!(h.gv.overlay.is_animating());
    assert!(h.gv.is_animating());
    h.gv.protocols.locks.locked = true;
    assert!(!h.gv.is_animating());
    h.gv.protocols.locks.locked = false;
    assert!(h.gv.is_animating());
}

#[test]
fn keyboard_resize_and_slot_changes_cancel_geometry_overrides() {
    let mut h = Harness::new();
    let window = h.window();
    h.gv.map_new_window(window.clone());
    let from = Rectangle::new((10, 20).into(), (100, 100).into());
    let to = Rectangle::new((30, 40).into(), (200, 200).into());
    h.gv.window_animations.entries[0]
        .motion
        .retarget(from.to_f64(), to.to_f64(), Duration::ZERO);
    h.gv.resize_direction(crate::snap::Direction::Right);
    let motion = h.gv.window_animations.motion(&window).unwrap();
    assert!(!motion.active());
    assert_eq!(motion.rect(to.to_f64()), to.to_f64());

    h.gv.window_animations.entries[0]
        .motion
        .retarget(from.to_f64(), to.to_f64(), Duration::ZERO);
    h.gv.configure_slot(&window, to, crate::window_mode::WindowMode::Floating);
    let motion = h.gv.window_animations.motion(&window).unwrap();
    assert!(!motion.active());
    assert_eq!(motion.rect(to.to_f64()), to.to_f64());
}

fn feature_output(h: &mut Harness) -> Output {
    let output = Output::new("feature-test".into(), smithay::output::PhysicalProperties {
        size: (0, 0).into(), subpixel: smithay::output::Subpixel::Unknown,
        make: "test".into(), model: "test".into(),
    });
    output.change_current_state(Some(smithay::output::Mode { size: (1600, 900).into(), refresh: 120_000 }), None, None, None);
    h.gv.space.map_output(&output, (0, 0));
    h.gv.workspaces.ensure(&output);
    output
}

fn crash_feature(h: &mut Harness, id: Feature, output: &Output) {
    h.gv.features.inject_panic(id);
    match id {
        Feature::Tiling => h.gv.reflow_tiling(false),
        Feature::Dock => h.gv.finish_dock_launches(),
        Feature::Bar => h.gv.refresh_shell(),
        Feature::Overview => h.gv.overview_motion((500.0, 400.0).into()),
        Feature::Snap => h.gv.snap_direction(crate::snap::Direction::Left),
        Feature::Switcher => h.gv.switcher_cycle(false),
        Feature::Gestures => { h.gv.super_tap_key(125, true, true, false); }
        Feature::WindowGestures => h.gv.begin_window_drag(1000),
        Feature::ResizeMode => h.gv.enter_resize_mode(),
        Feature::Animations => { h.gv.is_animating(); }
        Feature::Decorations => h.gv.focus_window(None),
        Feature::Ipc => { h.gv.guard(id, Ok(()), crate::ipc::init).unwrap(); }
        Feature::Screencopy => crate::handlers::screencopy::frame_drawn(&mut h.gv, output),
    }
    h.gv.run_feature_resets();
    h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
    assert!(matches!(h.gv.features.state(id), crate::module::FeatureState::Crashed { .. }), "{id:?}");
    assert!(!h.gv.features.on(id));
    assert!(h.gv.features.reset_count[id as usize] > 0, "{id:?}");
    assert!(h.gv.pointer_buttons.is_empty());
    assert!(!h.gv.seat.get_pointer().unwrap().is_grabbed());
    let count = h.gv.features.reset_count[id as usize];
    h.gv.guard(id, (), |_| panic!("crashed features must never run again"));
    assert_eq!(h.gv.features.reset_count[id as usize], count);
}

#[test]
fn every_feature_crash_is_contained_and_reset_in_a_production_entry() {
    for id in Feature::ALL {
        let mut h = Harness::new();
        let output = feature_output(&mut h);
        let w = h.window();
        h.gv.map_new_window(w.clone());
        h.gv.window_animations.entries.clear();
        crash_feature(&mut h, id, &output);
        assert!(h.gv.space.element_location(&w).is_some(), "{id:?} lost a window");
        assert!(!h.gv.protocols.locks.locked);
    }
}

#[test]
fn tiling_crash_keeps_two_windows_floating_and_inside_output() {
    let mut h = Harness::new();
    let output = feature_output(&mut h);
    let a = h.window(); let b = h.window();
    h.gv.map_new_window(a.clone()); h.gv.map_new_window(b.clone());
    h.gv.toggle_workspace_tiling(&output);
    h.gv.window_animations.entries.clear();
    assert!(h.gv.tiled(&a) && h.gv.tiled(&b));
    crash_feature(&mut h, Feature::Tiling, &output);
    let bounds = h.gv.space.output_geometry(&output).unwrap();
    for w in [&a, &b] {
        assert!(!h.gv.tiled(w));
        assert_eq!(crate::window_mode::mode_state(w).mode, crate::window_mode::WindowMode::Floating);
        let loc = h.gv.space.element_location(w).unwrap();
        let size = w.toplevel().unwrap().with_pending_state(|s| s.size.unwrap());
        assert!(bounds.contains_rect(Rectangle::new(loc, size)));
        assert!(crate::window_mode::mode_state(w).tile_restore.is_none());
    }
    assert!(h.gv.workspaces.get(&output).unwrap().list.items.iter().all(|ws| !ws.tiling));
}

#[test]
fn dock_crash_restores_minimized_windows_and_drops_only_its_reserve() {
    let mut h = Harness::new(); let output = feature_output(&mut h);
    let w = h.window(); h.gv.map_new_window(w.clone());
    let with_dock = h.gv.work_area(&output);
    h.gv.minimize_window(&w);
    assert!(crate::window_mode::mode_state(&w).minimized);
    crash_feature(&mut h, Feature::Dock, &output);
    assert!(!crate::window_mode::mode_state(&w).minimized);
    assert!(h.gv.space.element_location(&w).is_some());
    assert!(h.gv.work_area(&output).size.h > with_dock.size.h);
}

#[test]
fn overview_crash_removes_transform_and_restores_window_hit_testing() {
    let mut h = Harness::new(); let output = feature_output(&mut h);
    let w = h.window(); h.gv.map_new_window(w.clone()); h.commit_size(&w, 400, 300);
    h.gv.window_animations.entries.clear();
    h.gv.toggle_overview(); h.gv.overview.progress.jump(1.0);
    assert!(h.gv.overview.visible());
    crash_feature(&mut h, Feature::Overview, &output);
    assert!(!h.gv.overview.visible());
    let loc = h.gv.space.element_location(&w).unwrap().to_f64();
    let hit = h.gv.surface_under(loc + Point::from((20.0, 20.0))).unwrap();
    assert_eq!(&hit.0, w.toplevel().unwrap().wl_surface());
}

#[test]
fn animation_crash_finishes_target_geometry_and_idle_stays_idle() {
    let mut h = Harness::new(); let output = feature_output(&mut h);
    let w = h.window(); h.gv.map_new_window(w.clone());
    let from = Rectangle::new((100, 100).into(), (300, 200).into());
    let target = Rectangle::new((600, 350).into(), (500, 300).into());
    h.gv.space.map_element(w.clone(), target.loc, false);
    w.toplevel().unwrap().with_pending_state(|s| s.size = Some(target.size));
    crate::anim::animate_geometry(&mut h.gv, &w, from);
    assert!(h.gv.window_animations.is_animating());
    crash_feature(&mut h, Feature::Animations, &output);
    assert_eq!(h.gv.space.element_location(&w), Some(target.loc));
    assert!(h.gv.window_animations.entries.is_empty());
    assert!(!h.gv.is_animating());
}

#[test]
fn gesture_crash_settles_swipe_and_balances_pressed_buttons() {
    use smithay::input::pointer::{ButtonEvent, Focus, GrabStartData};
    let mut h = Harness::new(); let output = feature_output(&mut h);
    let w = h.window(); h.gv.map_new_window(w.clone()); h.gv.window_animations.entries.clear();
    let owner = h.gv.workspaces.ensure(&output);
    owner.held = true; owner.position.follow(0.4, Duration::ZERO);
    let pointer = h.gv.seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    pointer.set_grab(&mut h.gv, crate::grabs::MoveSurfaceGrab::start(GrabStartData {
        focus: None, button: 0x110, location: (500.0, 400.0).into(),
    }, w, (100, 100).into()), serial, Focus::Clear);
    h.gv.pointer_buttons.push(0x110);
    pointer.button(&mut h.gv, &ButtonEvent { button: 0x110, state: smithay::backend::input::ButtonState::Pressed, serial, time: 1 });
    crash_feature(&mut h, Feature::Gestures, &output);
    let owner = h.gv.workspaces.get(&output).unwrap();
    assert!(!owner.held); assert_eq!(owner.position.value, owner.list.active as f64);
    assert!(!h.gv.is_animating());
}

#[test]
fn split_borrow_crash_and_innermost_attribution() {
    let mut h = Harness::new();
    h.gv.features.inject_panic(Feature::Dock);
    assert!(!h.gv.guard(Feature::Bar, false, |gv| gv.guard(Feature::Dock, false, |_| true)));
    assert!(h.gv.features.on(Feature::Bar));
    assert!(!h.gv.features.on(Feature::Dock));
    h.gv.features.inject_panic(Feature::Overview);
    assert_eq!(h.gv.features.call(Feature::Overview, 42, || 7), 42);
    assert_eq!(h.gv.features.reset_count[Feature::Overview as usize], 0);
    h.gv.on_loop_idle();
    assert!(h.gv.features.reset_count[Feature::Overview as usize] > 0);
}

#[test]
fn all_config_switches_disable_features_without_reserves_or_idle_animation() {
    use crate::input::Action;
    for id in Feature::ALL {
        let key = match id {
            Feature::Dock => "dock", Feature::Bar => "bar", Feature::Overview => "overview",
            Feature::Tiling => "tiling", Feature::Snap => "snap", Feature::Switcher => "switcher",
            Feature::Gestures => "gestures", Feature::WindowGestures => "window_gestures",
            Feature::ResizeMode => "resize_mode", Feature::Animations => "animations",
            Feature::Decorations => "decorations", Feature::Ipc => "ipc", Feature::Screencopy => "screencopy",
        };
        let mut h = Harness::new();
        let output = feature_output(&mut h);
        h.gv.config = Config::build(true, Some(&format!("[modules]\n{key} = false\n[dock]\nenabled = false\n[bar]\nenabled = false")), |_| None).0;
        h.gv.features = Features::new(&h.gv.config);
        assert!(!h.gv.features.on(id));
        assert_eq!(h.gv.work_area(&output), h.gv.space.output_geometry(&output).unwrap());
        h.gv.guard(id, (), |_| panic!("disabled feature was called"));
        h.gv.on_loop_idle();
        assert!(!h.gv.is_animating(), "{id:?}");
        assert_eq!(h.gv.features.state(id), crate::module::FeatureState::Disabled);
        let mods = smithay::input::keyboard::ModifiersState { alt: true, ..Default::default() };
        use smithay::input::keyboard::{keysyms, Keysym};
        let binding = match id {
            Feature::Dock => Some((keysyms::KEY_m, mods, Action::Minimize)),
            Feature::Overview => Some((keysyms::KEY_a, mods, Action::ToggleOverview)),
            Feature::Tiling => Some((keysyms::KEY_t, mods, Action::ToggleTiling)),
            Feature::Snap => Some((keysyms::KEY_Left, smithay::input::keyboard::ModifiersState { ctrl: true, ..mods }, Action::Snap(crate::snap::Direction::Left))),
            Feature::Switcher => Some((keysyms::KEY_Tab, Default::default(), Action::SwitcherCycle(false))),
            Feature::ResizeMode => Some((keysyms::KEY_r, mods, Action::ResizeMode)),
            _ => None,
        };
        if let Some((key, mut mods, action)) = binding {
            if id == Feature::Switcher { mods.alt = true; h.gv.config.mod_key = crate::config::ModKey::Super; }
            assert_ne!(h.gv.bind_for(Keysym::from(key), &mods), Some(action));
        }
    }
}

#[test]
fn snap_off_has_no_drag_preview_and_a_snap_crash_clears_only_preview() {
    let mut h = Harness::new(); let output = feature_output(&mut h);
    let w = h.window(); h.gv.map_new_window(w.clone());
    h.gv.config.modules.snap = false;
    h.gv.features = Features::new(&h.gv.config);
    h.gv.update_snap_preview(&w, (0.0, 400.0).into());
    assert!(!h.gv.overlay.is_animating());
    h.gv.config.modules.snap = true;
    h.gv.features = Features::new(&h.gv.config);
    h.gv.update_snap_preview(&w, (0.0, 400.0).into());
    assert!(h.gv.overlay.is_animating());
    crash_feature(&mut h, Feature::Snap, &output);
    assert!(!h.gv.overlay.is_animating());
    assert!(h.gv.features.on(Feature::Switcher));
}

#[test]
fn null_buffer_commit_unmaps_and_drops_focus() {
    let mut h = Harness::new();
    let _output = feature_output(&mut h);
    let w = h.window();
    h.gv.map_new_window(w.clone());
    h.commit_size(&w, 400, 300);
    h.gv.focus_window(Some(&w));
    assert_eq!(h.gv.focused_window().as_ref(), Some(&w));
    let surface = h.surfaces.iter().find(|(x, _)| x == &w).unwrap().1.clone();
    surface.attach(None, 0, 0);
    surface.commit();
    h.exchange();
    h.exchange();
    assert!(h.gv.space.element_location(&w).is_none());
    assert!(h.gv.focused_window().is_none());
    assert!(h.gv.unmapped.contains(&w), "the toplevel waits to be mapped again");
}
