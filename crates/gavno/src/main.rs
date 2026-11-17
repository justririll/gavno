//! GAVNO: a lightweight hybrid Wayland compositor.
//!
//! Phase 1: floating windows with anchored move/resize, a DRM/KMS backend for
//! running on a TTY and a winit backend for running nested during development.
//! Rendering is on demand: when nothing changes, GAVNO does not draw.

mod perf;
mod anim;
mod classic;
mod decor;
mod desktop_entry;
mod dock;
mod icons;
mod topbar;
mod backend;
mod config;
mod config_schema;
mod config_reload;
mod code_mode;
mod hints;
mod keyboard_overlay;
mod brightness;
#[cfg(test)]
mod config_schema_tests;
mod crash_log;
mod color;
mod night_light;
mod hdr;
mod cursor;
mod debug;
mod greeter;
mod lock;
mod power;
mod launcher;
mod launch;
mod notify;
mod portal;
mod screensaver;
mod media;
mod polkit;
mod grabs;
mod handlers;
mod input;
mod ipc;
mod module;
mod overlay;
mod snap;
mod render;
mod render_sanitize;
mod state;
mod ui;
mod window_mode;
mod workspace;
mod tiling;
mod overview;
mod gesture;
mod window_gesture;
mod window_input;
mod resize_mode;
mod xwayland;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

pub use state::Gavno;

/// Cap glibc malloc at two arenas. Every helper thread (Mesa shader queues,
/// D-Bus workers) otherwise gets its own arena; in the compositor that was
/// ~58 MB of idle RSS. An explicit MALLOC_ARENA_MAX still wins.
/// Return freed heap pages to the kernel after a large cache is dropped
/// (leaving Classic). glibc keeps them otherwise.
pub(crate) fn release_free_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases free allocator pages.
    unsafe {
        libc::malloc_trim(0);
    }
}

fn limit_malloc_arenas() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        // SAFETY: mallopt only adjusts allocator tuning; called before any thread starts.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, 2);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("msg") {
        return ipc::run_client(std::env::args().skip(2));
    }
    perf::start_process();
    limit_malloc_arenas();
    init_logging();
    perf::configure_stalls();
    perf::startup("process.started");
    crash_log::install();

    let mut args = std::env::args().skip(1);
    let mut backend_choice = None;
    let mut startup_cmd: Option<String> = None;
    let mut greeter_mode = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--winit" => backend_choice = Some(BackendChoice::Winit),
            "--tty" => backend_choice = Some(BackendChoice::Tty),
            "--greeter" => greeter_mode = true,
            "-c" | "--command" => startup_cmd = args.next(),
            "-h" | "--help" => {
                println!("usage: gavno [--tty | --winit] [--greeter] [-c <command>]");
                println!("       gavno --version");
                println!("       gavno msg <method> [json]");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("gavno {}", env!("CARGO_PKG_VERSION"));
                println!("License GPL-3.0-or-later <https://gnu.org/licenses/gpl.html>.");
                println!("This is free software: you are free to change and redistribute it.");
                println!("There is NO WARRANTY, to the extent permitted by law.");
                return Ok(());
            }
            other => {
                eprintln!("gavno: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    // Nested when started inside another Wayland or X11 session, on the TTY otherwise.
    let backend_choice = backend_choice.unwrap_or_else(|| {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some() {
            BackendChoice::Winit
        } else {
            BackendChoice::Tty
        }
    });

    let mut event_loop: EventLoop<'static, Gavno> = EventLoop::try_new()?;
    let dispatch_start = perf::watch_dispatch(&event_loop.handle())?;
    let display: Display<Gavno> = Display::new()?;
    perf::startup("config.begin");
    let config = config::Config::load(backend_choice == BackendChoice::Winit);
    perf::startup("config.loaded");
    perf::startup("gavno.new.begin");
    let mut state = Gavno::new(&mut event_loop, display, config);
    ipc::debug_input::init(&mut state);

    perf::startup("gavno.new.end");

    match backend_choice {
        BackendChoice::Winit => backend::winit::init(&mut event_loop, &mut state)?,
        BackendChoice::Tty => backend::udev::init(&mut event_loop, &mut state)?,
    }
    perf::startup("backend.ready");
    // The window IPC lets agents drive the desktop; never on the login screen.
    if !greeter_mode {
        lock::start(&mut state);
        perf::startup("screencopy.init.begin");
        state.guard(module::Feature::Screencopy, (), handlers::screencopy::init);
        perf::startup("screencopy.init.end");
        perf::startup("ipc.init.begin");
        if let Err(err) = state.guard(module::Feature::Ipc, Ok(()), ipc::init) {
            tracing::warn!(?err, "window IPC disabled");
        }
        perf::startup("ipc.init.end");
    }

    if greeter_mode {
        greeter::start(&mut state);
    }
    debug::install_screenshot_signal(&state.loop_handle);

    // Clients we start must talk to us, never to a parent session.
    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
    std::env::remove_var("DISPLAY");
    std::env::set_var("XDG_CURRENT_DESKTOP", "GAVNO");
    std::env::set_var("XDG_SESSION_DESKTOP", "GAVNO");
    std::env::set_var("XDG_SESSION_TYPE", "wayland");
    // Before any client or autostart command is spawned, so all inherit it.
    if !greeter_mode && backend_choice == BackendChoice::Tty {
        disable_accessibility_bus(&state.config);
    }
    if !greeter_mode {
        xwayland::start(&mut state);
        launch::init(&mut state);
        start_polkit_agent(&mut state, backend_choice == BackendChoice::Tty);
        // A nested compositor must not repoint the host session's services.
        if backend_choice == BackendChoice::Tty {
            export_session_environment(state.config.display.accessibility);
            screensaver::start(&mut state);
            portal::start(&mut state);
            autostart(&mut state);
        }
    }
    tracing::info!(socket = ?state.socket_name, "gavno is listening");
    perf::startup("listening");
    if !greeter_mode {
        icons::warm_app_icons(state.config.dock.icon_theme.clone());
    }

    // Clients such as foot quit when there is no output yet, so the startup
    // command waits for the first one (the TTY backend may still be waiting
    // for its VT).
    state.startup_cmd = startup_cmd;
    if state.space.outputs().next().is_some() {
        state.output_added_hook();
    }

    event_loop.run(None, &mut state, |state| {
        {
            let _stall = perf::Stall::new("calloop.on_loop_idle");
            state.on_loop_idle();
        }
        perf::finish_dispatch(&dispatch_start);
    })?;
    if backend::udev::panel_kept_on_exit() {
        tracing::info!("handing the panel to the session without DRM teardown");
        // No destructors: they would restore SDR and the boot-time KMS state.
        std::process::exit(0);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendChoice {
    Winit,
    Tty,
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(filter))
        .with(perf::DrmStartup.with_filter(tracing_subscriber::filter::dynamic_filter_fn(|metadata, _| {
            perf::drm_startup_trace() && metadata.target().starts_with("smithay::backend::drm")
        })))
        .init();
}

/// Stops GTK and Qt apps from starting the AT-SPI bus (about 25 MB of RSS)
/// unless `[display] accessibility` is on.
const ACCESSIBILITY_VARS: [(&str, &str); 2] = [("GTK_A11Y", "none"), ("NO_AT_BRIDGE", "1")];
fn disable_accessibility_bus(config: &config::Config) {
    if !config.display.accessibility {
        for (name, value) in ACCESSIBILITY_VARS {
            std::env::set_var(name, value);
        }
    }
}

/// D-Bus activated and systemd user services (xdg-desktop-portal and its
/// wlr backend for screen sharing, for example) need our display variables.
fn export_session_environment(accessibility: bool) {
    let _startup = perf::Startup::new("session_environment.begin", "session_environment.end");
    let mut names = vec!["WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE"];
    if !accessibility {
        names.extend(ACCESSIBILITY_VARS.iter().map(|(name, _)| *name));
    }
    if std::env::var_os("DISPLAY").is_some() {
        names.push("DISPLAY");
    }
    state::spawn_shell(&format!(
        "dbus-update-activation-environment --systemd {0} 2>/dev/null || systemctl --user import-environment {0}",
        names.join(" ")
    ));
}

/// Starts the session polkit agent when it is installed, so pkexec and other
/// polkit prompts work. It registers for the session and waits for requests.
/// `polkit_agent = "builtin"` registers the compositor's own agent, another
/// value spawns that external agent, and an empty value runs none. A nested
/// compositor shares the host session, whose agent already owns it, so the
/// built-in one registers there only when GAVNO_POLKIT_TEST is set, and with
/// GAVNO_POLKIT_TEST_PROCESS=<pid> it serves only that process.
fn start_polkit_agent(state: &mut Gavno, tty: bool) {
    let _startup = crate::perf::Startup::new("polkit.start.begin", "polkit.start.end");
    let command = state.config.polkit_agent.trim().to_owned();
    if command.is_empty() {
        return;
    }
    if command == "builtin" {
        if tty || std::env::var_os("GAVNO_POLKIT_TEST").is_some() {
            let process = std::env::var("GAVNO_POLKIT_TEST_PROCESS").ok().and_then(|p| p.trim().parse().ok());
            state.start_polkit(polkit::Target { process, ..Default::default() });
        }
        return;
    }
    if !spawn_if_installed(&command) {
        tracing::info!(%command, "polkit agent not installed; polkit prompts are unavailable");
    }
}

/// Runs `command` through the shell when its program exists.
fn spawn_if_installed(command: &str) -> bool {
    let Some(program) = command.split_whitespace().next() else {
        return false;
    };
    let found = installed(program);
    if found {
        state::spawn_shell(command);
    }
    found
}

/// Starts the configured resident helpers. With a foot server running, the
/// default terminal becomes `footclient`, which opens a window in a few
/// milliseconds instead of starting a new process.
fn autostart(state: &mut Gavno) {
    let _startup = crate::perf::Startup::new("autostart.begin", "autostart.end");
    let mut foot_server = false;
    for command in state.config.autostart.clone() {
        let started = spawn_if_installed(&command);
        tracing::info!(target: "gavno::startup", stage = "autostart.spawn", ms = perf::elapsed_ms(), %command, started);
        let mut words = command.split_whitespace();
        if started && words.next() == Some("foot") && words.any(|w| w == "--server" || w == "-s") {
            foot_server = true;
        }
    }
    if foot_server && state.config.terminal == "foot" && installed("footclient") {
        state.config.terminal = "footclient --app-id foot".into();
    }
}

/// Whether `program` (a path, or a name looked up in PATH) is a file.
fn installed(program: &str) -> bool {
    if program.contains('/') {
        return std::path::Path::new(program).is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())
    })
}
