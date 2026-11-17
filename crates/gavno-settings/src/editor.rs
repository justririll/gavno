//! Lazy, schema-driven editor. Only the current page has widgets. Writes are
//! serialized and coalesced by key; each worker exits as soon as its job ends.
use crate::{
    config::{self, Change},
    ipc,
    model::{self, Kind, Spec},
    work,
};
use adw::prelude::*;
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use toml_edit::DocumentMut;

struct Editor {
    window: adw::ApplicationWindow,
    overlay: adw::ToastOverlay,
    banner: adw::Banner,
    stack: adw::ViewStack,
    sidebar: gtk::ListBox,
    body: gtk::Box,
    search: gtk::SearchEntry,
    specs: RefCell<Vec<Spec>>,
    document: RefCell<DocumentMut>,
    outputs: RefCell<Vec<Value>>,
    keyboard: RefCell<Value>,
    online: Cell<bool>,
    page: RefCell<String>,
    query: RefCell<String>,
    busy: Rc<Cell<bool>>,
    saving: Cell<bool>,
    pending: RefCell<BTreeMap<Vec<String>, Value>>,
    raw_pending: RefCell<Option<String>>,
    text_edits: RefCell<BTreeMap<Vec<String>, Value>>,
    draft: RefCell<Option<String>>,
    raw_error: RefCell<Option<(glib::WeakRef<gtk::Label>, glib::WeakRef<gtk::TextView>)>>,
    closed: Cell<bool>,
    last_toast: RefCell<glib::WeakRef<adw::Toast>>,
    raw_dirty: Cell<bool>,
}
struct Startup {
    specs: Vec<Spec>,
    document: DocumentMut,
    outputs: Vec<Value>,
    keyboard: Value,
    online: bool,
    error: Option<String>,
}

pub fn open(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Settings")
        .default_width(1000)
        .default_height(740)
        .build();
    let overlay = adw::ToastOverlay::new();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Settings", "GAVNO")));
    toolbar.add_top_bar(&header);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let banner = adw::Banner::new("GAVNO is not running; changes apply at next login");
    root.append(&banner);
    let search = gtk::SearchEntry::new();
    search.set_search_delay(0);
    search.set_placeholder_text(Some("Search settings by key or description"));
    search.set_margin_start(12);
    search.set_margin_end(12);
    search.set_margin_top(12);
    search.set_margin_bottom(12);
    root.append(&search);
    let split = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    split.set_vexpand(true);
    let sidebar = gtk::ListBox::new();
    sidebar.add_css_class("navigation-sidebar");
    sidebar.set_width_request(205);
    sidebar.set_selection_mode(gtk::SelectionMode::Single);
    let side_scroll = gtk::ScrolledWindow::builder()
        .child(&sidebar)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    split.append(&side_scroll);
    let stack = adw::ViewStack::new();
    stack.set_hexpand(true);
    let body = crate::page();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    stack.add_titled(&scroll, Some("content"), "Settings");
    split.append(&stack);
    root.append(&split);
    toolbar.set_content(Some(&root));
    overlay.set_child(Some(&toolbar));
    window.set_content(Some(&overlay));
    let editor = Rc::new(Editor {
        window: window.clone(),
        overlay,
        banner,
        stack,
        sidebar: sidebar.clone(),
        body,
        search: search.clone(),
        specs: RefCell::new(model::fallback().unwrap_or_default()),
        document: RefCell::new(DocumentMut::new()),
        outputs: RefCell::new(Vec::new()),
        keyboard: RefCell::new(json!({})),
        online: Cell::new(false),
        page: RefCell::new("Displays".into()),
        query: RefCell::new(String::new()),
        busy: Rc::new(Cell::new(false)),
        saving: Cell::new(false),
        pending: RefCell::new(BTreeMap::new()),
        raw_pending: RefCell::new(None),
        text_edits: RefCell::new(BTreeMap::new()),
        draft: RefCell::new(None),
        raw_error: RefCell::new(None),
        closed: Cell::new(false),
        last_toast: RefCell::new(glib::WeakRef::new()),
        raw_dirty: Cell::new(false),
    });
    editor.rebuild_sidebar();
    let weak = Rc::downgrade(&editor);
    sidebar.connect_row_selected(move |_, row| {
        let Some(e) = weak.upgrade() else { return };
        let Some(label) = row.and_then(|r| r.child()).and_downcast::<gtk::Label>() else {
            return;
        };
        let previous = e.page.replace(label.text().to_string());
        if !e.search.text().is_empty() {
            e.search.set_text("");
        }
        if previous != *e.page.borrow()
            && ["Displays", "Keyboard"].contains(&previous.as_str())
            && !e.busy.get()
        {
            let page = e.page.borrow().clone();
            let e = e.clone();
            glib::spawn_future_local(async move {
                if let Ok(Ok(text)) = work::run(config::read).await {
                    if let Ok(doc) = model::validate(&text, &e.specs.borrow()) {
                        if !e.saving.get() {
                            *e.document.borrow_mut() = doc;
                        }
                    }
                }
                if *e.page.borrow() == page && !e.closed.get() {
                    e.show();
                }
            });
        } else {
            e.show();
        }
    });
    let weak = Rc::downgrade(&editor);
    sidebar.connect_row_activated(move |_, row| {
        let Some(e) = weak.upgrade() else { return };
        if !e.search.text().is_empty() {
            if let Some(label) = row.child().and_downcast::<gtk::Label>() {
                *e.page.borrow_mut() = label.text().to_string();
            }
            e.search.set_text("");
        }
    });
    let weak = Rc::downgrade(&editor);
    search.connect_search_changed(move |entry| {
        if let Some(e) = weak.upgrade() {
            *e.query.borrow_mut() = entry.text().to_string();
            e.show();
        }
    });
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Reload settings and connected devices"));
    header.pack_start(&refresh);
    let weak = Rc::downgrade(&editor);
    refresh.connect_clicked(move |_| {
        if let Some(e) = weak.upgrade() {
            if !e.saving.get() {
                e.load();
            }
        }
    });
    let lifetime = RefCell::new(Some(editor.clone()));
    window.connect_close_request(move |_| {
        if let Some(e) = lifetime.borrow_mut().take() {
            e.closed.set(true);
            e.flush_text_edits();
        }
        glib::Propagation::Proceed
    });
    sidebar.select_row(sidebar.row_at_index(0).as_ref());
    editor.load();
    window.present();
}
impl Editor {
    fn rebuild_sidebar(&self) {
        let mut pages = vec!["Displays".to_owned(), "Keyboard".to_owned()];
        for spec in self.specs.borrow().iter() {
            if !pages.contains(&spec.section) {
                pages.push(spec.section.clone());
            }
        }
        pages.push("Edit config file".into());
        while let Some(child) = self.sidebar.first_child() {
            self.sidebar.remove(&child);
        }
        let current = self.page.borrow().clone();
        let selected = pages.iter().position(|page| page == &current).unwrap_or(0);
        for name in pages {
            let row = gtk::ListBoxRow::new();
            let label = gtk::Label::new(Some(&name));
            label.set_xalign(0.0);
            label.set_margin_start(12);
            label.set_margin_end(12);
            label.set_margin_top(10);
            label.set_margin_bottom(10);
            row.set_child(Some(&label));
            self.sidebar.append(&row);
        }
        self.sidebar
            .select_row(self.sidebar.row_at_index(selected as i32).as_ref());
    }
    fn toast(&self, text: impl AsRef<str>) {
        // Rapid edits replace feedback rather than queueing idle expiry work.
        if let Some(previous) = self.last_toast.borrow().upgrade() {
            previous.dismiss();
        }
        let toast = adw::Toast::new(text.as_ref());
        *self.last_toast.borrow_mut() = toast.downgrade();
        self.overlay.add_toast(toast);
    }
    fn load(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        let e = self.clone();
        glib::spawn_future_local(async move {
            let result = work::run(|| -> Result<Startup, String> {
                let schema = ipc::call("config_schema", &json!({}));
                let online = schema.is_ok();
                let specs = match schema {
                    Ok(v) => model::schema(v)?,
                    Err(_) => model::fallback()?,
                };
                let text = config::read()?;
                let (document, error) = match model::validate(&text, &specs) {
                    Ok(doc) => (doc, None),
                    Err(error) => (DocumentMut::new(), Some(error)),
                };
                let outputs = if online {
                    ipc::call("outputs", &json!({}))
                        .ok()
                        .and_then(|v| v.as_array().cloned())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let keyboard = if online {
                    ipc::call("keyboard", &json!({})).unwrap_or_else(|_| json!({}))
                } else {
                    json!({})
                };
                Ok(Startup {
                    specs,
                    document,
                    outputs,
                    keyboard,
                    online,
                    error,
                })
            })
            .await
            .and_then(|r| r);
            let interacting =
                e.saving.get() || !e.text_edits.borrow().is_empty() || e.raw_dirty.get();
            e.busy.set(e.saving.get());
            match result {
                Ok(state) => {
                    *e.specs.borrow_mut() = state.specs;
                    if !interacting {
                        *e.document.borrow_mut() = state.document;
                    }
                    *e.outputs.borrow_mut() = state.outputs;
                    *e.keyboard.borrow_mut() = state.keyboard;
                    e.online.set(state.online);
                    e.banner.set_revealed(!state.online);
                    if let Some(error) = state.error {
                        e.toast(error);
                        *e.page.borrow_mut() = "Edit config file".into();
                    }
                }
                Err(error) => {
                    e.online.set(false);
                    e.banner.set_revealed(true);
                    e.toast(error);
                }
            }
            if !e.closed.get() && !interacting {
                e.rebuild_sidebar();
            }
        });
    }
    fn show(self: &Rc<Self>) {
        self.flush_text_edits();
        if self.raw_dirty.replace(false) {
            if let Some((_, view)) = self.raw_error.borrow().as_ref() {
                if let Some(view) = view.upgrade() {
                    let buffer = view.buffer();
                    *self.draft.borrow_mut() = Some(
                        buffer
                            .text(&buffer.start_iter(), &buffer.end_iter(), true)
                            .to_string(),
                    );
                }
            }
        }
        *self.raw_error.borrow_mut() = None;
        crate::clear(&self.body);
        let query = self.query.borrow().trim().to_ascii_lowercase();
        if !query.is_empty() {
            self.search_rows(&query);
            return;
        }
        let name = self.page.borrow().clone();
        match name.as_str() {
            "Displays" => {
                if !self.online.get() {
                    self.body.append(&gtk::Label::new(Some("Edit Output defaults or Per-output overrides to configure displays offline.")));
                } else {
                    let e = self.clone();
                    glib::spawn_future_local(async move {
                        let result = work::run(|| ipc::call("outputs", &json!({})))
                            .await
                            .and_then(|r| r);
                        if e.closed.get()
                            || *e.page.borrow() != "Displays"
                            || !e.query.borrow().is_empty()
                        {
                            return;
                        }
                        match result {
                            Ok(outputs) => {
                                *e.outputs.borrow_mut() =
                                    outputs.as_array().cloned().unwrap_or_default();
                                crate::clear(&e.body);
                                for output in e.outputs.borrow().iter() {
                                    crate::display_row(
                                        &e.window,
                                        &e.overlay,
                                        &e.body,
                                        &e.stack,
                                        &e.busy,
                                        output.clone(),
                                    );
                                }
                            }
                            Err(error) => {
                                if ipc::unavailable(&error) {
                                    e.online.set(false);
                                    e.banner.set_revealed(true);
                                }
                                e.toast(error);
                            }
                        }
                    });
                }
            }
            "Keyboard" => {
                let e = self.clone();
                glib::spawn_future_local(async move {
                    let layouts = work::run(|| {
                        let state = ipc::call("keyboard", &json!({})).ok();
                        crate::xkb::load().map(|layouts| (layouts, state))
                    })
                    .await
                    .and_then(|r| r);
                    if e.closed.get()
                        || *e.page.borrow() != "Keyboard"
                        || !e.query.borrow().is_empty()
                    {
                        return;
                    }
                    match layouts {
                        Ok((layouts, state)) => {
                            if let Some(state) = state {
                                *e.keyboard.borrow_mut() = state;
                            }
                            let doc = e.document.borrow();
                            let defaults = |key: &str, fallback: &str| {
                                model::lookup(&doc, &["keyboard".into(), key.into()])
                                    .as_str()
                                    .unwrap_or(fallback)
                                    .to_owned()
                            };
                            let mut state = e.keyboard.borrow().clone();
                            state["layout"] = json!(defaults("layout", "us,ru"));
                            state["variant"] = json!(defaults("variant", ""));
                            state["options"] = json!(defaults("options", "grp:alt_shift_toggle"));
                            crate::keyboard_page(
                                &e.window, &e.overlay, &e.body, &e.stack, &e.busy, state, layouts,
                            );
                        }
                        Err(error) => e.toast(error),
                    }
                });
            }
            "Edit config file" => self.raw_page(),
            "Per-output overrides" => self.outputs_page(),
            section => {
                let specs = self
                    .specs
                    .borrow()
                    .iter()
                    .filter(|s| s.section == section)
                    .cloned()
                    .collect::<Vec<_>>();
                let group = adw::PreferencesGroup::builder().title(section).build();
                self.body.append(&group);
                for spec in specs {
                    let path = spec.path.split('.').map(str::to_owned).collect();
                    group.add(&self.row(spec, path));
                }
            }
        }
    }
    fn search_rows(self: &Rc<Self>, query: &str) {
        let matches = self
            .specs
            .borrow()
            .iter()
            .filter(|s| {
                s.path.to_ascii_lowercase().contains(query)
                    || s.description.to_ascii_lowercase().contains(query)
            })
            .cloned()
            .collect::<Vec<_>>();
        let group = adw::PreferencesGroup::builder()
            .title("Search results")
            .build();
        self.body.append(&group);
        let mut count = 0;
        for spec in matches {
            if spec.path.contains(".*.") {
                for name in self.output_names() {
                    if count >= 64 {
                        break;
                    }
                    let path = vec![
                        "outputs".into(),
                        name,
                        spec.path.rsplit('.').next().unwrap_or("").into(),
                    ];
                    group.add(&self.row(spec.clone(), path));
                    count += 1;
                }
            } else {
                let path = spec.path.split('.').map(str::to_owned).collect();
                group.add(&self.row(spec, path));
                count += 1;
            }
            if count >= 64 {
                self.body.append(&gtk::Label::new(Some(
                    "Showing the first 64 matches; refine your search.",
                )));
                break;
            }
        }
        if count == 0 {
            self.body
                .append(&gtk::Label::new(Some("No matching settings")));
        }
    }
    fn output_names(&self) -> Vec<String> {
        let mut names = self
            .document
            .borrow()
            .get("outputs")
            .and_then(|i| i.as_table_like())
            .map(|t| {
                t.iter()
                    .take(128)
                    .map(|(k, _)| k.to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for output in self.outputs.borrow().iter() {
            if let Some(name) = output["name"].as_str() {
                if names.len() < 128 && !names.iter().any(|n| n == name) {
                    names.push(name.into());
                }
            }
        }
        names.sort();
        names
    }
    fn outputs_page(self: &Rc<Self>) {
        let names = self.output_names();
        let refs = names.iter().map(String::as_str).collect::<Vec<_>>();
        let chooser = gtk::DropDown::from_strings(&refs);
        chooser.set_enable_search(true);
        self.body.append(&chooser);
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 12);
        self.body.append(&box_);
        let render = |e: &Rc<Self>, name: &str, box_: &gtk::Box| {
            crate::clear(box_);
            let group = adw::PreferencesGroup::builder().title(name).build();
            box_.append(&group);
            for spec in e
                .specs
                .borrow()
                .iter()
                .filter(|s| s.path.contains(".*."))
                .cloned()
                .collect::<Vec<_>>()
            {
                let path = vec![
                    "outputs".into(),
                    name.into(),
                    spec.path.rsplit('.').next().unwrap_or("").into(),
                ];
                group.add(&e.row(spec, path));
            }
        };
        if let Some(name) = names.first() {
            render(self, name, &box_);
        }
        let weak = Rc::downgrade(self);
        let boxes = box_.clone();
        chooser.connect_selected_notify(move |row| {
            if let (Some(e), Some(name)) = (weak.upgrade(), names.get(row.selected() as usize)) {
                render(&e, name, &boxes);
            }
        });
        let add = adw::EntryRow::builder()
            .title("Add connector (for example eDP-1)")
            .show_apply_button(true)
            .build();
        let weak = Rc::downgrade(self);
        add.connect_apply(move |row| {
            let Some(e) = weak.upgrade() else { return };
            let name = row.text();
            if name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            {
                e.toast("Enter a connector name such as eDP-1");
                return;
            }
            if e.output_names().len() >= 128 {
                e.toast("Use the config file editor for additional connectors");
                return;
            }
            e.outputs.borrow_mut().push(json!({"name":name.as_str()}));
            e.show();
        });
        let add_group = adw::PreferencesGroup::new();
        add_group.add(&add);
        self.body.append(&add_group);
    }
    fn row(self: &Rc<Self>, spec: Spec, path: Vec<String>) -> gtk::Widget {
        // The GTK widget retains its static text/layout; Rust runs only when a
        // control changes, never on a frame callback.
        let stored = model::lookup(&self.document.borrow(), &path);
        let current = if stored.is_null() {
            spec.default.clone()
        } else {
            stored
        };
        let mut displayed = current.clone();
        if displayed.is_null() && path.first().is_some_and(|p| p == "outputs") {
            let document = self.document.borrow();
            displayed = match path.last().map(String::as_str) {
                Some("enabled") => json!(true),
                Some("hdr") => json!(model::lookup(&document, &["output".into(), "hdr".into()])
                    .as_bool()
                    .unwrap_or(false)),
                Some("scale") => {
                    let internal = path.get(1).is_some_and(|n| {
                        ["eDP-", "LVDS-", "DSI-"].iter().any(|p| n.starts_with(p))
                    });
                    json!(if internal {
                        model::lookup(&document, &["output".into(), "internal_scale".into()])
                            .as_f64()
                            .unwrap_or(1.75)
                    } else {
                        1.0
                    })
                }
                Some("mode") => model::lookup(&document, &["output".into(), "mode".into()]),
                _ => Value::Null,
            };
        }
        let title = path.join(".");
        let description = spec.description.clone();
        let reset = gtk::Button::from_icon_name("edit-undo-symbolic");
        reset.add_css_class("flat");
        reset.set_valign(gtk::Align::Center);
        reset.set_tooltip_text(Some("Reset to default / inherited value"));
        reset.set_visible(current != spec.default);
        let weak = Rc::downgrade(self);
        let reset_path = path.clone();
        let default = spec.default.clone();
        reset.connect_clicked(move |_| {
            if let Some(e) = weak.upgrade() {
                e.text_edits.borrow_mut().remove(&reset_path);
                e.queue(reset_path.clone(), default.clone());
                e.show();
            }
        });
        let weak = Rc::downgrade(self);
        let reset_weak = reset.downgrade();
        let default = spec.default.clone();
        let entry_path = path.clone();
        let list_path = path.clone();
        let commit: Rc<dyn Fn(Value)> = Rc::new(move |value: Value| {
            if let Some(button) = reset_weak.upgrade() {
                button.set_visible(value != default);
            }
            if let Some(e) = weak.upgrade() {
                e.queue(path.clone(), value);
            }
        });
        match spec.control() {
            model::Control::Switch => {
                let row = adw::SwitchRow::builder()
                    .title(&title)
                    .subtitle(&description)
                    .active(displayed.as_bool().unwrap_or(false))
                    .build();
                row.add_suffix(&reset);
                if !spec.live {
                    row.add_suffix(&relogin_tag());
                }
                row.connect_active_notify(move |row| commit(json!(row.is_active())));
                row.upcast()
            }
            model::Control::Spin => {
                let (min, max, step, digits) = match spec.kind {
                    Kind::Int { min, max, step } => (
                        min as f64,
                        (max as f64).min(9_007_199_254_740_991.0),
                        step as f64,
                        0,
                    ),
                    Kind::Float { min, max, step } => (min, max, step, 2),
                    _ => (0.0, 1.0, 1.0, 0),
                };
                let adjustment = gtk::Adjustment::new(
                    displayed.as_f64().unwrap_or(min),
                    min,
                    max,
                    step,
                    step * 10.0,
                    0.0,
                );
                let row = adw::SpinRow::builder()
                    .title(&title)
                    .subtitle(&description)
                    .adjustment(&adjustment)
                    .digits(digits)
                    .build();
                row.add_suffix(&reset);
                if !spec.live {
                    row.add_suffix(&relogin_tag());
                }
                row.connect_value_notify(move |row| {
                    if digits == 0 {
                        commit(json!(row.value().round() as i64));
                    } else {
                        commit(json!(row.value()));
                    }
                });
                row.upcast()
            }
            model::Control::Entry => {
                let box_ = gtk::Box::new(gtk::Orientation::Vertical, 0);
                let entry_text = displayed.as_str().map(str::to_owned).unwrap_or_else(|| {
                    if displayed.is_number() {
                        displayed.to_string()
                    } else {
                        String::new()
                    }
                });
                let row = adw::EntryRow::builder()
                    .title(
                        if matches!(spec.kind, Kind::Float { .. }) && displayed.is_null() {
                            format!("{title} (use time zone)")
                        } else {
                            title.clone()
                        },
                    )
                    .show_apply_button(true)
                    .text(&entry_text)
                    .build();
                row.add_suffix(&reset);
                if !spec.live {
                    row.add_suffix(&relogin_tag());
                }
                let label = gtk::Label::new(Some(&description));
                label.add_css_class("dim-label");
                label.set_wrap(true);
                label.set_xalign(0.0);
                label.set_margin_start(12);
                label.set_margin_end(12);
                label.set_margin_bottom(8);
                let entries = gtk::ListBox::new();
                entries.set_selection_mode(gtk::SelectionMode::None);
                entries.append(&row);
                box_.append(&entries);
                box_.append(&label);
                let weak = Rc::downgrade(self);
                let changed_path = entry_path.clone();
                let pending_reset = reset.downgrade();
                let default = spec.default.clone();
                row.connect_changed(move |row| {
                    let value = model::entry_value(&spec.kind, row.text().as_str());
                    if let Some(button) = pending_reset.upgrade() {
                        button.set_visible(value != default);
                    }
                    if let Some(e) = weak.upgrade() {
                        e.text_edits
                            .borrow_mut()
                            .insert(changed_path.clone(), value);
                    }
                });
                let weak = Rc::downgrade(self);
                let applied_path = entry_path.clone();
                row.connect_apply(move |_| {
                    if let Some(e) = weak.upgrade() {
                        e.commit_text(&applied_path);
                    }
                });
                let focus = gtk::EventControllerFocus::new();
                let weak = Rc::downgrade(self);
                focus.connect_leave(move |_| {
                    if let Some(e) = weak.upgrade() {
                        e.commit_text(&entry_path);
                    }
                });
                row.add_controller(focus);
                let wrapper = adw::PreferencesRow::builder().title(&title).build();
                wrapper.set_activatable(false);
                wrapper.set_child(Some(&box_));
                wrapper.upcast()
            }
            model::Control::Color => {
                let Kind::Color { living } = spec.kind else {
                    unreachable!()
                };
                let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);
                let row = adw::EntryRow::builder()
                    .title(&title)
                    .show_apply_button(true)
                    .text(
                        displayed
                            .as_str()
                            .filter(|s| *s != "living")
                            .unwrap_or("#7aa2ff"),
                    )
                    .build();
                row.add_suffix(&reset);
                let choices = gtk::StringList::new(&["Living", "Custom color"]);
                if living {
                    let choice = adw::ComboRow::builder()
                        .title("Accent source")
                        .subtitle(&description)
                        .model(&choices)
                        .build();
                    choice.set_selected(if displayed == "living" { 0 } else { 1 });
                    row.set_sensitive(displayed != "living");
                    let entry = row.downgrade();
                    let commit = commit.clone();
                    let weak = Rc::downgrade(self);
                    let source_path = entry_path.clone();
                    choice.connect_selected_notify(move |choice| {
                        if let Some(e) = weak.upgrade() {
                            e.text_edits.borrow_mut().remove(&source_path);
                        }
                        if let Some(entry) = entry.upgrade() {
                            let custom = choice.selected() == 1;
                            entry.set_sensitive(custom);
                            commit(if custom {
                                json!(entry.text().as_str())
                            } else {
                                json!("living")
                            });
                        }
                    });
                    box_.append(&choice);
                }
                // A static color swatch updates on editing; there is no preview timer.
                let preview = gtk::DrawingArea::new();
                preview.set_content_height(24);
                let entry = row.downgrade();
                preview.set_draw_func(move |_, cr, w, h| {
                    let text = entry
                        .upgrade()
                        .map(|r| r.text().to_string())
                        .unwrap_or_default();
                    let text = text.strip_prefix('#').unwrap_or("");
                    if let Ok(color) = u32::from_str_radix(text, 16) {
                        cr.set_source_rgb(
                            f64::from((color >> 16) & 255) / 255.0,
                            f64::from((color >> 8) & 255) / 255.0,
                            f64::from(color & 255) / 255.0,
                        );
                        cr.rectangle(0.0, 0.0, f64::from(w), f64::from(h));
                        let _ = cr.fill();
                    }
                });
                let weak_preview = preview.downgrade();
                row.connect_changed(move |_| {
                    if let Some(preview) = weak_preview.upgrade() {
                        preview.queue_draw();
                    }
                });
                let changed_path = entry_path.clone();
                let weak = Rc::downgrade(self);
                row.connect_changed(move |row| {
                    if let Some(e) = weak.upgrade() {
                        e.text_edits
                            .borrow_mut()
                            .insert(changed_path.clone(), json!(row.text().as_str()));
                    }
                });
                let weak = Rc::downgrade(self);
                row.connect_apply(move |_| {
                    if let Some(e) = weak.upgrade() {
                        e.commit_text(&entry_path);
                    }
                });
                box_.append(&row);
                box_.append(&preview);
                let wrapper = adw::PreferencesRow::builder().title(&title).build();
                wrapper.set_activatable(false);
                wrapper.set_child(Some(&box_));
                wrapper.upcast()
            }
            model::Control::Combo => {
                let Kind::Enum { values } = spec.kind else {
                    return gtk::Label::new(None).upcast();
                };
                let refs = values.iter().map(String::as_str).collect::<Vec<_>>();
                let row = adw::ComboRow::builder()
                    .title(&title)
                    .subtitle(&description)
                    .model(&gtk::StringList::new(&refs))
                    .build();
                row.set_selected(values.iter().position(|v| current == *v).unwrap_or(0) as u32);
                row.add_suffix(&reset);
                if !spec.live {
                    row.add_suffix(&relogin_tag());
                }
                row.connect_selected_notify(move |row| {
                    if let Some(value) = values.get(row.selected() as usize) {
                        commit(json!(value));
                    }
                });
                row.upcast()
            }
            model::Control::List => {
                let row = adw::ExpanderRow::builder()
                    .title(&title)
                    .subtitle(&description)
                    .build();
                row.add_suffix(&reset);
                if !spec.live {
                    row.add_suffix(&relogin_tag());
                }
                let box_ = gtk::Box::new(gtk::Orientation::Vertical, 0);
                row.add_row(&box_);
                let weak = Rc::downgrade(self);
                let default = spec.default.clone();
                row.connect_expanded_notify(move |row| {
                    crate::clear(&box_);
                    if row.is_expanded() {
                        let value = weak
                            .upgrade()
                            .map(|e| model::lookup(&e.document.borrow(), &list_path))
                            .filter(|v| !v.is_null())
                            .unwrap_or_else(|| default.clone());
                        let values = Rc::new(RefCell::new(
                            value
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default(),
                        ));
                        list_rows(&box_, &values, &commit, 0);
                    }
                });
                row.upcast()
            }
        }
    }
    fn commit_text(self: &Rc<Self>, path: &[String]) {
        let value = self.text_edits.borrow_mut().remove(path);
        if let Some(value) = value {
            self.queue(path.to_vec(), value);
        }
    }
    fn flush_text_edits(self: &Rc<Self>) {
        let edits = std::mem::take(&mut *self.text_edits.borrow_mut());
        for (path, value) in edits {
            self.queue(path, value);
        }
    }
    fn queue(self: &Rc<Self>, path: Vec<String>, value: Value) {
        let change = Change {
            path: path.clone(),
            value: value.clone(),
        };
        if let Err(error) = config::set_value(&mut self.document.borrow_mut(), &change) {
            self.toast(error);
            return;
        }
        self.pending.borrow_mut().insert(path, value);
        self.start_save();
    }
    fn start_save(self: &Rc<Self>) {
        if self.saving.replace(true) {
            return;
        }
        self.busy.set(true);
        let e = self.clone();
        let hold = e.window.application().map(|app| app.hold());
        glib::spawn_future_local(async move {
            let _hold = hold;
            loop {
                let raw = e.raw_pending.borrow_mut().take();
                let changes = std::mem::take(&mut *e.pending.borrow_mut())
                    .into_iter()
                    .map(|(path, value)| Change { path, value })
                    .collect::<Vec<_>>();
                if raw.is_none() && changes.is_empty() {
                    break;
                }
                let specs = e.specs.borrow().clone();
                let saved_raw = raw.clone();
                let result = work::run(move || save_job(raw, changes, &specs))
                    .await
                    .and_then(|r| r);
                match result {
                    Ok(saved) => {
                        if saved_raw.is_some() {
                            if let Some((label, _)) = e.raw_error.borrow().as_ref() {
                                if let Some(label) = label.upgrade() {
                                    label.set_text("");
                                }
                            }
                            if saved.online && !saved.needs_login {
                                e.toast("Configuration saved");
                            }
                        }
                        e.online.set(saved.online);
                        e.banner.set_revealed(!saved.online);
                        if let Ok(doc) = model::validate(&saved.text, &e.specs.borrow()) {
                            *e.document.borrow_mut() = doc;
                        }
                        // Changes typed during I/O remain visible in the model.
                        for (path, value) in e.pending.borrow().iter() {
                            let _ = config::set_value(
                                &mut e.document.borrow_mut(),
                                &Change {
                                    path: path.clone(),
                                    value: value.clone(),
                                },
                            );
                        }
                        if saved_raw
                            .as_ref()
                            .is_some_and(|s| e.draft.borrow().as_ref() == Some(s))
                        {
                            *e.draft.borrow_mut() = None;
                        }
                        if saved.needs_login || !saved.online {
                            e.toast("Takes effect after re-login");
                        }
                    }
                    Err(error) => {
                        e.show_error(&error);
                        if let Ok(Ok(text)) = work::run(config::read).await {
                            if let Ok(doc) = model::validate(&text, &e.specs.borrow()) {
                                *e.document.borrow_mut() = doc;
                            }
                        }
                    }
                }
            }
            e.busy.set(false);
            e.saving.set(false);
        });
    }
    fn raw_page(self: &Rc<Self>) {
        let text = self
            .draft
            .borrow()
            .clone()
            .unwrap_or_else(|| self.document.borrow().to_string());
        let view = gtk::TextView::builder()
            .monospace(true)
            .wrap_mode(gtk::WrapMode::None)
            .vexpand(true)
            .build();
        let buffer = view.buffer();
        buffer.set_text(&text);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .min_content_height(400)
            .vexpand(true)
            .build();
        self.body.append(&scroll);
        let error = gtk::Label::new(None);
        error.set_wrap(true);
        error.set_xalign(0.0);
        error.add_css_class("error");
        self.body.append(&error);
        *self.raw_error.borrow_mut() = Some((error.downgrade(), view.downgrade()));
        buffer.connect_insert_text(|buffer, _, text| {
            if buffer.char_count() as usize + text.chars().count() > 1024 * 1024 {
                buffer.stop_signal_emission_by_name("insert-text");
            }
        });
        let weak = Rc::downgrade(self);
        buffer.connect_changed(move |_| {
            if let Some(e) = weak.upgrade() {
                e.raw_dirty.set(true);
            }
        });
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        save.set_halign(gtk::Align::End);
        self.body.append(&save);
        let weak = Rc::downgrade(self);
        save.connect_clicked(move |_| {
            if let Some(e) = weak.upgrade() {
                let text = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), true)
                    .to_string();
                *e.draft.borrow_mut() = Some(text.clone());
                *e.raw_pending.borrow_mut() = Some(text);
                e.pending.borrow_mut().clear();
                e.start_save();
            }
        });
        let help = gtk::Label::new(Some(
            "Save validates TOML and keeps the previous version in config.toml.bak.",
        ));
        help.add_css_class("dim-label");
        help.set_wrap(true);
        self.body.append(&help);
        // If the file was invalid on load, retain its original text for repair.
        if self.document.borrow().as_table().is_empty() && self.draft.borrow().is_none() {
            let weak = Rc::downgrade(self);
            let buffer = view.buffer();
            glib::spawn_future_local(async move {
                if let Ok(Ok(text)) = work::run(config::read).await {
                    if let Some(e) = weak.upgrade() {
                        if *e.page.borrow() == "Edit config file" && e.draft.borrow().is_none() {
                            buffer.set_text(&text);
                        }
                    }
                }
            });
        }
    }
    fn show_error(&self, error: &str) {
        self.toast(error);
        if let Some((label, view)) = self.raw_error.borrow().as_ref() {
            if let Some(label) = label.upgrade() {
                label.set_text(error);
            }
            if let Some(view) = view.upgrade() {
                if let Some(line) = error
                    .split("line ")
                    .nth(1)
                    .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|s| s.parse::<i32>().ok())
                {
                    let buffer = view.buffer();
                    if let Some(mut start) = buffer.iter_at_line((line - 1).max(0)) {
                        let mut end = start;
                        end.forward_to_line_end();
                        buffer.select_range(&start, &end);
                        view.scroll_to_iter(&mut start, 0.15, false, 0.0, 0.0);
                    }
                }
            }
        }
    }
}
fn list_rows(
    box_: &gtk::Box,
    values: &Rc<RefCell<Vec<String>>>,
    commit: &Rc<dyn Fn(Value)>,
    offset: usize,
) {
    crate::clear(box_);
    let len = values.borrow().len();
    let entries = gtk::ListBox::new();
    entries.set_selection_mode(gtk::SelectionMode::None);
    box_.append(&entries);
    for index in offset..(offset + 32).min(len) {
        let row = adw::EntryRow::builder()
            .title(format!("Item {}", index + 1))
            .text(&values.borrow()[index])
            .build();
        let values2 = values.clone();
        let commit2 = commit.clone();
        row.connect_changed(move |row| {
            let mut values = values2.borrow_mut();
            if let Some(value) = values.get_mut(index) {
                *value = row.text().to_string();
                commit2(json!(*values));
            }
        });
        for (icon, delta) in [
            ("go-up-symbolic", -1_isize),
            ("go-down-symbolic", 1),
            ("user-trash-symbolic", 0),
        ] {
            let button = gtk::Button::from_icon_name(icon);
            button.add_css_class("flat");
            button.set_valign(gtk::Align::Center);
            button.set_sensitive(match delta {
                -1 => index > 0,
                1 => index + 1 < len,
                _ => true,
            });
            let weak_box = box_.downgrade();
            let values = values.clone();
            let commit = commit.clone();
            button.connect_clicked(move |_| {
                let mut list = values.borrow_mut();
                if index >= list.len() {
                    return;
                }
                if delta == 0 {
                    list.remove(index);
                } else {
                    let to = (index as isize + delta) as usize;
                    if to >= list.len() {
                        return;
                    }
                    list.swap(index, to);
                }
                let value = json!(*list);
                drop(list);
                commit(value);
                if let Some(box_) = weak_box.upgrade() {
                    list_rows(
                        &box_,
                        &values,
                        &commit,
                        offset.min(values.borrow().len().saturating_sub(1) / 32 * 32),
                    );
                }
            });
            row.add_suffix(&button);
        }
        entries.append(&row);
    }
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for (label, next) in [
        ("Previous", offset.saturating_sub(32)),
        ("Next", offset + 32),
    ] {
        let button = gtk::Button::with_label(label);
        button.set_sensitive(if label == "Previous" {
            offset > 0
        } else {
            next < len
        });
        let weak_box = box_.downgrade();
        let values = values.clone();
        let commit = commit.clone();
        button.connect_clicked(move |_| {
            if let Some(box_) = weak_box.upgrade() {
                list_rows(&box_, &values, &commit, next);
            }
        });
        controls.append(&button);
    }
    let add = gtk::Button::with_label("Add item");
    let weak_box = box_.downgrade();
    let values = values.clone();
    let commit = commit.clone();
    add.connect_clicked(move |_| {
        values.borrow_mut().push(String::new());
        commit(json!(*values.borrow()));
        if let Some(box_) = weak_box.upgrade() {
            let offset = values.borrow().len().saturating_sub(1) / 32 * 32;
            list_rows(&box_, &values, &commit, offset);
        }
    });
    controls.append(&add);
    box_.append(&controls);
}
struct Saved {
    text: String,
    online: bool,
    needs_login: bool,
}
fn save_job(raw: Option<String>, changes: Vec<Change>, specs: &[Spec]) -> Result<Saved, String> {
    let (old, text) = if let Some(text) = raw {
        let old = config::save_raw(&text, specs)?;
        (old, text)
    } else {
        config::save_changes(&changes, specs)?
    };
    match ipc::call("reload_config", &json!({})) {
        Ok(result) => Ok(Saved {
            text,
            online: true,
            needs_login: result["needs_relogin"]
                .as_array()
                .is_some_and(|a| !a.is_empty()),
        }),
        Err(error) if ipc::unavailable(&error) => Ok(Saved {
            text,
            online: false,
            needs_login: true,
        }),
        Err(error) => {
            config::restore(&old)
                .map_err(|restore| format!("{error}; restoring config failed: {restore}"))?;
            Err(error)
        }
    }
}

fn relogin_tag() -> gtk::Label {
    let label = gtk::Label::new(Some("after re-login"));
    label.add_css_class("relogin-tag");
    label.set_valign(gtk::Align::Center);
    label
}
