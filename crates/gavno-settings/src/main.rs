mod config;
#[path = "../../gavno/src/config_schema.rs"]
mod config_schema;
mod editor;
mod ipc;
mod model;
mod transaction;
mod work;
mod xkb;

use adw::prelude::*;
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

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
    let app = adw::Application::builder()
        .application_id("org.gavno.Settings")
        .build();
    app.connect_startup(|_| {
        // Static settings pages need no caret-blink redraw timer.
        if let Some(settings) = gtk::Settings::default() {
            settings.set_gtk_cursor_blink(false);
        }
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("style.css"));
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        editor::open(app);
    });
    app.run()
}
fn message(overlay: &adw::ToastOverlay, text: impl AsRef<str>) {
    overlay.add_toast(adw::Toast::new(text.as_ref()));
}
fn page() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 20);
    page.add_css_class("settings-page");
    page
}
fn clear(page: &gtk::Box) {
    while let Some(child) = page.first_child() {
        page.remove(&child);
    }
}
fn mode_label(mode: &Value) -> String {
    format!(
        "{} × {} · {:.3} Hz{}",
        mode["w"],
        mode["h"],
        mode["refresh"].as_f64().unwrap_or(0.0) / 1000.0,
        if mode["preferred"] == true {
            " · preferred"
        } else {
            ""
        }
    )
}
fn display_row(
    window: &adw::ApplicationWindow,
    overlay: &adw::ToastOverlay,
    page: &gtk::Box,
    stack: &adw::ViewStack,
    busy: &Rc<Cell<bool>>,
    output: Value,
) {
    let name = output["name"].as_str().unwrap_or("Unknown").to_owned();
    let group = adw::PreferencesGroup::builder()
        .title(&name)
        .description(format!(
            "{} {}{} · position {}, {}",
            output["make"].as_str().unwrap_or(""),
            output["model"].as_str().unwrap_or(""),
            if output["internal"] == true {
                " · built-in"
            } else {
                ""
            },
            output["position"]["x"],
            output["position"]["y"]
        ))
        .build();
    page.append(&group);
    if output["enabled"] != true {
        group.add(
            &adw::ActionRow::builder()
                .title("Disabled in configuration")
                .subtitle(
                    "Edit enabled in config.toml and restart the session to enable this display.",
                )
                .build(),
        );
        return;
    }
    let modes = output["modes"].as_array().cloned().unwrap_or_default();
    let labels: Vec<_> = modes.iter().map(mode_label).collect();
    let choices: Vec<_> = labels.iter().map(String::as_str).collect();
    let model = gtk::StringList::new(&choices);
    let mode = adw::ComboRow::builder()
        .title("Resolution and refresh rate")
        .model(&model)
        .build();
    if let Some(index) = modes.iter().position(|m| {
        ["w", "h", "refresh"]
            .iter()
            .all(|k| m[k] == output["mode"][k])
    }) {
        mode.set_selected(index as u32);
    }
    group.add(&mode);
    let scale = gtk::SpinButton::with_range(0.5, 3.0, 0.05);
    scale.set_digits(2);
    scale.set_value(output["scale"].as_f64().unwrap_or(1.0));
    scale.set_valign(gtk::Align::Center);
    let row = adw::ActionRow::builder()
        .title("Scale")
        .subtitle("Logical size of text and controls")
        .build();
    row.add_suffix(&scale);
    group.add(&row);
    let presets = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for n in [1.0, 1.25, 1.5, 1.75, 2.0] {
        let button = gtk::Button::with_label(&format!("{n}"));
        let scale = scale.clone();
        button.connect_clicked(move |_| scale.set_value(n));
        presets.append(&button);
    }
    let row = adw::ActionRow::builder().title("Common scales").build();
    row.add_suffix(&presets);
    group.add(&row);
    let hdr = adw::SwitchRow::builder()
        .title("HDR")
        .subtitle("High dynamic range")
        .active(
            output["hdr"]["pending"]
                .as_bool()
                .unwrap_or(output["hdr"]["enabled"] == true),
        )
        .build();
    let supported = output["hdr"]["supported"] == true;
    if supported {
        group.add(&hdr);
    }
    let apply = gtk::Button::with_label("Apply");
    apply.add_css_class("suggested-action");
    apply.set_halign(gtk::Align::End);
    page.append(&apply);
    let window = window.clone();
    let overlay = overlay.clone();
    let stack = stack.clone();
    let busy = busy.clone();
    let page = page.clone();
    apply.connect_clicked(move |_| {
        if busy.replace(true) { return; }
        let Some(selected) = modes.get(mode.selected() as usize) else { busy.set(false); return; };
        let mut params = json!({"output":name,"mode":{"w":selected["w"],"h":selected["h"],"refresh":selected["refresh"]},"scale":(scale.value()*20.0).round()/20.0});
        if supported { params["hdr"]=json!(hdr.is_active()); }
        let window = window.clone(); let overlay = overlay.clone(); let stack = stack.clone(); let busy = busy.clone(); let page = page.clone();
        glib::spawn_future_local(async move {
            stack.set_sensitive(false);
            match preview(&window, params).await {
                Ok(true) => message(&overlay, "Display settings saved"),
                Ok(false) => message(&overlay, "Previous display settings restored"),
                Err(e) => message(&overlay, e),
            }
            clear(&page);
            match ipc::request("outputs", json!({})).await {
                Ok(outputs) => if let Some(outputs) = outputs.as_array() { for o in outputs { display_row(&window, &overlay, &page, &stack, &busy, o.clone()); } },
                Err(e) => message(&overlay, e),
            }
            stack.set_sensitive(true); busy.set(false);
        });
    });
}
/// A worker owns the rollback deadline. It runs even if GTK is stalled or the
/// parent window closes. The application hold keeps it alive until completion.
async fn preview(window: &adw::ApplicationWindow, params: Value) -> Result<bool, String> {
    let app = window.application().ok_or("Application is closing")?;
    let _hold = app.hold();
    let (decision, decisions) = std::sync::mpsc::channel();
    let (ready, prepared) = futures_channel::oneshot::channel();
    let (done, finished) = futures_channel::oneshot::channel();
    let close_decision = decision.clone();
    let closed = Rc::new(Cell::new(false));
    let closing = closed.clone();
    let close = window.connect_close_request(move |_| {
        closing.set(true);
        let _ = close_decision.send(false);
        glib::Propagation::Proceed
    });
    std::thread::spawn(move || {
        let result = transaction::run(params, ready, decisions);
        let _ = done.send(result);
    });
    if let Some(start) = prepared.await.ok().filter(|_| !closed.get()) {
        let dialog = adw::AlertDialog::builder()
            .heading("Keep these settings?")
            .body("Reverting automatically in 15 seconds.")
            .build();
        dialog.add_responses(&[("revert", "Revert"), ("keep", "Keep")]);
        dialog.set_close_response("revert");
        dialog.set_default_response(Some("revert"));
        dialog.set_response_appearance("keep", adw::ResponseAppearance::Suggested);
        let tick_dialog = dialog.clone();
        let timeout = glib::timeout_add_local(Duration::from_millis(200), move || {
            let remaining = 15_u64.saturating_sub(start.elapsed().as_secs());
            tick_dialog.set_body(&format!("Reverting automatically in {remaining} seconds."));
            if remaining == 0 {
                tick_dialog.close();
            }
            glib::ControlFlow::Continue
        });
        let response = dialog.choose_future(Some(window)).await;
        timeout.remove();
        let _ = decision.send(response == "keep");
    }
    let result = finished
        .await
        .map_err(|_| "Display worker stopped".to_string())?;
    window.disconnect(close);
    result
}

#[derive(Clone)]
struct SelectedLayout {
    code: String,
    variant: String,
}
fn redraw_layouts(
    list: &gtk::ListBox,
    selected: &Rc<RefCell<Vec<SelectedLayout>>>,
    layouts: &Rc<Vec<xkb::Layout>>,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    for (index, item) in selected.borrow().iter().enumerate() {
        let title = layouts
            .iter()
            .find(|l| l.code == item.code)
            .map(|l| l.name.as_str())
            .unwrap_or(&item.code);
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(if item.variant.is_empty() {
                item.code.clone()
            } else {
                format!("{} ({})", item.code, item.variant)
            })
            .build();
        for (label, offset) in [
            ("go-up-symbolic", -1_isize),
            ("go-down-symbolic", 1),
            ("user-trash-symbolic", 0),
        ] {
            let button = gtk::Button::from_icon_name(label);
            button.set_valign(gtk::Align::Center);
            button.add_css_class("flat");
            button.set_tooltip_text(Some(match offset {
                -1 => "Move up",
                1 => "Move down",
                _ => "Remove layout",
            }));
            button.set_sensitive(match offset {
                -1 => index > 0,
                1 => index + 1 < selected.borrow().len(),
                _ => selected.borrow().len() > 1,
            });
            let list = list.clone();
            let selected = selected.clone();
            let layouts = layouts.clone();
            button.connect_clicked(move |_| {
                {
                    let mut selected = selected.borrow_mut();
                    if offset == 0 {
                        selected.remove(index);
                    } else {
                        selected.swap(index, (index as isize + offset) as usize);
                    }
                }
                redraw_layouts(&list, &selected, &layouts);
            });
            row.add_suffix(&button);
        }
        list.append(&row);
    }
}
fn keyboard_page(
    window: &adw::ApplicationWindow,
    overlay: &adw::ToastOverlay,
    page: &gtk::Box,
    stack: &adw::ViewStack,
    busy: &Rc<Cell<bool>>,
    state: Value,
    layouts: Vec<xkb::Layout>,
) {
    let layouts = Rc::new(layouts);
    let variants: Vec<_> = state["variant"]
        .as_str()
        .unwrap_or("")
        .split(',')
        .map(str::to_owned)
        .collect();
    let selected = Rc::new(RefCell::new(
        state["layout"]
            .as_str()
            .unwrap_or("us")
            .split(',')
            .enumerate()
            .map(|(i, code)| SelectedLayout {
                code: code.into(),
                variant: variants.get(i).cloned().unwrap_or_default(),
            })
            .collect::<Vec<_>>(),
    ));
    let group = adw::PreferencesGroup::builder()
        .title("Keyboard layouts")
        .description(format!(
            "Active group: {} · {}",
            state["active_group"],
            state["group_names"]
                .as_array()
                .map(|names| names
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ))
        .build();
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    redraw_layouts(&list, &selected, &layouts);
    group.add(&list);
    page.append(&group);
    let labels: Vec<_> = layouts
        .iter()
        .map(|l| format!("{} — {}", l.code, l.name))
        .collect();
    let refs: Vec<_> = labels.iter().map(String::as_str).collect();
    let chooser = gtk::DropDown::from_strings(&refs);
    chooser.set_enable_search(true);
    chooser.set_hexpand(true);
    let add = gtk::Button::with_label("Add layout");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&chooser);
    row.append(&add);
    page.append(&row);
    {
        let selected = selected.clone();
        let layouts = layouts.clone();
        let list = list.clone();
        let overlay = overlay.clone();
        add.connect_clicked(move |_| {
            if selected.borrow().len() >= 4 {
                message(&overlay, "XKB supports up to four layouts");
                return;
            }
            if let Some(layout) = layouts.get(chooser.selected() as usize) {
                if selected.borrow().iter().any(|l| l.code == layout.code) {
                    return;
                }
                selected.borrow_mut().push(SelectedLayout {
                    code: layout.code.clone(),
                    variant: String::new(),
                });
                redraw_layouts(&list, &selected, &layouts);
            }
        });
    }
    let options = state["options"].as_str().unwrap_or("").to_owned();
    let shortcuts = [
        "",
        "grp:alt_shift_toggle",
        "grp:win_space_toggle",
        "grp:caps_toggle",
    ];
    let shortcut = adw::ComboRow::builder()
        .title("Switch layout shortcut")
        .model(&gtk::StringList::new(&[
            "None",
            "Alt + Shift",
            "Super + Space",
            "Caps Lock",
        ]))
        .build();
    shortcut.set_selected(
        shortcuts
            .iter()
            .position(|s| options.split(',').any(|o| o == *s))
            .unwrap_or(0) as u32,
    );
    let group = adw::PreferencesGroup::new();
    group.add(&shortcut);
    page.append(&group);
    let apply = gtk::Button::with_label("Apply and save");
    apply.add_css_class("suggested-action");
    apply.set_halign(gtk::Align::End);
    page.append(&apply);
    let window = window.clone();
    let overlay = overlay.clone();
    let stack = stack.clone();
    let busy = busy.clone();
    apply.connect_clicked(move |_| {
        if busy.replace(true) { return; }
        let mut options: Vec<_> = options.split(',').filter(|s| !s.is_empty() && !s.starts_with("grp:")).collect();
        if let Some(option) = shortcuts.get(shortcut.selected() as usize).filter(|s| !s.is_empty()) { options.push(option); }
        let params = json!({"layout":selected.borrow().iter().map(|s| s.code.as_str()).collect::<Vec<_>>().join(","),
            "variant":selected.borrow().iter().map(|s| s.variant.as_str()).collect::<Vec<_>>().join(","),"options":options.join(",")});
        let window = window.clone(); let overlay = overlay.clone(); let stack = stack.clone(); let busy = busy.clone();
        glib::spawn_future_local(async move {
            let _hold = window.application().map(|app| app.hold()); stack.set_sensitive(false);
            let result = work::run(move || {
                match ipc::call("set_keyboard", &params) {
                    Ok(_) => config::save("set_keyboard", &params).map(|_| false),
                    Err(error) if ipc::unavailable(&error) => config::save("set_keyboard", &params).map(|_| true),
                    Err(error) => Err(error),
                }
            }).await.and_then(|result|result);
            match result {
                Ok(false) => message(&overlay,"Keyboard settings saved"),
                Ok(true) => message(&overlay,"Takes effect after re-login"),
                Err(error) => message(&overlay,error),
            }
            stack.set_sensitive(true); busy.set(false);
        });
    });
}
