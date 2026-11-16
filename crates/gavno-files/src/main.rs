mod clipboard;
mod config;
mod dbus;
mod model;
mod ops;
mod preview;
mod search;
mod sidebar;
mod thumbs;
mod views;
mod window;
use adw::prelude::*;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.gavno.Files")
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_startup(|app| {
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
    app.connect_activate(|app| {
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
    app.run()
}
