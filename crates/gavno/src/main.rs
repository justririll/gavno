//! GAVNO: a lightweight hybrid Wayland compositor.
//!
//! Phase 1: floating windows with anchored move/resize, a DRM/KMS backend for
//! running on a TTY and a winit backend for running nested during development.
//! Rendering is on demand: when nothing changes, GAVNO does not draw.

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
#[cfg(test)]
mod config_schema_tests;
mod crash_log;
mod color;
mod hdr;
mod cursor;
mod debug;
mod greeter;
mod lock;
mod launcher;
mod notify;
mod media;
mod grabs;
mod handlers;
mod input;
mod ipc;
mod module;
mod overlay;
mod snap;
mod render;
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
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
    let display: Display<Gavno> = Display::new()?;
    let config = config::Config::load(backend_choice == BackendChoice::Winit);
    let mut state = Gavno::new(&mut event_loop, display, config);

    match backend_choice {
        BackendChoice::Winit => backend::winit::init(&mut event_loop, &mut state)?,
        BackendChoice::Tty => backend::udev::init(&mut event_loop, &mut state)?,
    }
    // The window IPC lets agents drive the desktop; never on the login screen.
    if !greeter_mode {
        lock::start(&mut state);
        state.guard(module::Feature::Screencopy, (), handlers::screencopy::init);
        if let Err(err) = state.guard(module::Feature::Ipc, Ok(()), ipc::init) {
            tracing::warn!(?err, "window IPC disabled");
        }
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
    if !greeter_mode {
        xwayland::start(&mut state);
        start_polkit_agent(&state.config.polkit_agent);
        // A nested compositor must not repoint the host session's services.
        if backend_choice == BackendChoice::Tty {
            export_session_environment();
            autostart(&mut state);
        }
    }
    tracing::info!(socket = ?state.socket_name, "gavno is listening");

    // Clients such as foot quit when there is no output yet, so the startup
    // command waits for the first one (the TTY backend may still be waiting
    // for its VT).
    state.startup_cmd = startup_cmd;
    if state.space.outputs().next().is_some() {
        state.output_added_hook();
    }

    event_loop.run(None, &mut state, |state| state.on_loop_idle())?;
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
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// D-Bus activated and systemd user services (xdg-desktop-portal and its
/// wlr backend for screen sharing, for example) need our display variables.
fn export_session_environment() {
    let mut names = vec!["WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE"];
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
fn start_polkit_agent(command: &str) {
    if !spawn_if_installed(command) {
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
    let mut foot_server = false;
    for command in state.config.autostart.clone() {
        let started = spawn_if_installed(&command);
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
