mod clipboard;
mod config;
mod dock;
mod icons;
mod dbus;
mod model;
mod polkit;
mod root;
// The helper binary uses the server half; the UI only the types.
#[allow(dead_code)]
mod root_proto;
mod ops;
mod preview;
mod search;
mod sidebar;
mod thumbs;
mod views;
mod window;
use adw::prelude::*;

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
    // `--daemon` keeps a windowless instance resident so later launches only
    // ask it over D-Bus for a window instead of starting GTK again.
    let mut args: Vec<String> = std::env::args().collect();
    let daemon = args.iter().any(|a| a == "--daemon");
    args.retain(|a| a != "--daemon");
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
    let skip_first = std::cell::Cell::new(daemon);
    let resident = std::cell::RefCell::new(None);
    app.connect_activate(move |app| {
        if skip_first.replace(false) {
            resident.replace(Some(app.hold()));
            return;
        }
        window::open(app, gio::File::for_path(glib::home_dir()), Vec::new());
    });
    app.connect_open(|app, files, _| {
        for file in files {
            let app = app.clone();
            let file = file.clone();
            let hold = app.hold();
            glib::spawn_future_local(async move {
                let _hold = hold;
                match file
                    .query_info_future(
                        model::ATTRIBUTES,
                        gio::FileQueryInfoFlags::NONE,
                        glib::Priority::DEFAULT,
                    )
                    .await
                {
                    Ok(info) if model::is_dir(&info) => {
                        window::open(&app, file, Vec::new());
                    }
                    Ok(_) => {
                        if let Some(parent) = file.parent() {
                            window::open(&app, parent, vec![file.uri().to_string()]);
                        }
                    }
                    Err(e) => {
                        window::open(&app, file, Vec::new()).notify(&e.to_string());
                    }
                }
            });
        }
    });
    app.run_with_args(&args)
}
