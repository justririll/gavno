mod clipboard;
mod config;
mod dbus;
mod dock;
mod icons;
mod model;
mod open;
mod ops;
mod polkit;
mod preview;
mod root;
// The helper binary uses the server half; the UI only the types.
#[allow(dead_code)]
mod root_proto;
mod search;
mod sidebar;
mod thumbs;
mod trash;
mod views;
mod window;

use adw::prelude::*;
use model::Location;

/// Cap glibc malloc at two arenas. Every helper thread (Mesa shader queues,
/// D-Bus workers) otherwise gets its own arena; in the compositor that was
/// ~58 MB of idle RSS. An explicit MALLOC_ARENA_MAX still wins.
fn limit_malloc_arenas() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        // SAFETY: mallopt only adjusts allocator tuning; called before any thread starts.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, 2);
        }
    }
}

fn main() -> glib::ExitCode {
    limit_malloc_arenas();
    let raw_args: Vec<String> = std::env::args().collect();
    let mut args = Vec::with_capacity(raw_args.len());
    let mut daemon = false;
    let mut after_sep = false;
    for (idx, arg) in raw_args.into_iter().enumerate() {
        if idx > 0 && !after_sep && arg == "--" {
            after_sep = true;
            args.push(arg);
        } else if idx > 0 && !after_sep && arg == "--daemon" {
            daemon = true;
        } else {
            args.push(arg);
        }
    }

    let app = adw::Application::builder()
        .application_id("org.gavno.Files")
        .flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::CAN_OVERRIDE_APP_ID)
        .build();

    app.connect_startup(|app| {
        gtk::Window::set_default_icon_name("org.gavno.Files");
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("style.css"));
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        dbus::install(app);
    });

    let skip_first = std::rc::Rc::new(std::cell::Cell::new(daemon));
    let resident = std::rc::Rc::new(std::cell::RefCell::new(None));
    {
        let skip_first = skip_first.clone();
        let resident = resident.clone();
        app.connect_activate(move |app| {
            if skip_first.replace(false) {
                resident.replace(Some(app.hold()));
                return;
            }
            window::open(app, Location::Local(glib::home_dir()), Vec::new());
        });
    }

    app.connect_open(move |app, files, _| {
        if skip_first.replace(false) && resident.borrow().is_none() {
            resident.replace(Some(app.hold()));
        }
        for file in files {
            match Location::from_gio_file(file) {
                Ok(Location::Trash) => {
                    window::open(app, Location::Trash, Vec::new());
                }
                Ok(Location::Local(path)) => {
                    let app = app.clone();
                    let hold = app.hold();
                    glib::spawn_future_local(async move {
                        let _hold = hold;
                        let p = path.clone();
                        let is_item = gio::spawn_blocking(move || {
                            std::fs::metadata(&p)
                                .map(|m| !m.is_dir())
                                .unwrap_or_else(|_| std::fs::symlink_metadata(&p).is_ok())
                        })
                        .await
                        .unwrap_or(false);
                        if is_item {
                            if let Some(parent) = path.parent() {
                                window::open(
                                    &app,
                                    Location::Local(parent.to_path_buf()),
                                    vec![model::file_uri_from_path(&path)],
                                );
                                return;
                            }
                        }
                        window::open(&app, Location::Local(path), Vec::new());
                    });
                }
                Err(e) => {
                    window::open(app, Location::Local(glib::home_dir()), Vec::new()).notify(&e);
                }
            }
        }
    });

    app.run_with_args(&args)
}
