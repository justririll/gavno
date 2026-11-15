//! GAVNO: a lightweight hybrid Wayland compositor.
//!
//! Phase 1: floating windows with anchored move/resize, a DRM/KMS backend for
//! running on a TTY and a winit backend for running nested during development.
//! Rendering is on demand: when nothing changes, GAVNO does not draw.

mod anim;
mod decor;
mod backend;
mod config;
mod cursor;
mod debug;
mod greeter;
mod grabs;
mod handlers;
mod input;
mod ipc;
mod module;
mod render;
mod state;
mod ui;
mod window_mode;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

pub use state::Gavno;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

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
    let config = config::Config::from_env(backend_choice == BackendChoice::Winit);
    let mut state = Gavno::new(&mut event_loop, display, config);

    match backend_choice {
        BackendChoice::Winit => backend::winit::init(&mut event_loop, &mut state)?,
        BackendChoice::Tty => backend::udev::init(&mut event_loop, &mut state)?,
    }
    // The window IPC lets agents drive the desktop; never on the login screen.
    if !greeter_mode {
        if let Err(err) = ipc::init(&mut state) {
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
