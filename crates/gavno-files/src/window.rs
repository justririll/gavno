use crate::{
    clipboard,
    config::{Config, ViewMode},
    model,
    ops::{self, Operations},
    preview, search, sidebar,
    thumbs::Thumbnails,
    views::{Report, Views},
};
use adw::prelude::*;
use gtk::gdk;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
};

thread_local! {static WINDOWS:RefCell<Vec<Rc<FileWindow>>>=const{RefCell::new(Vec::new())};}

pub struct FileWindow {
    pub window: adw::ApplicationWindow,
    app: adw::Application,
    config: Rc<RefCell<Config>>,
    config_revision: Cell<u64>,
    config_saving: Cell<bool>,
    toast: adw::ToastOverlay,
    store: gio::ListStore,
    filter: gtk::CustomFilter,
    views: Views,
    current: RefCell<gio::File>,
    back: RefCell<Vec<gio::File>>,
    forward: RefCell<Vec<gio::File>>,
    back_button: gtk::Button,
    forward_button: gtk::Button,
    up_button: gtk::Button,
    path_stack: gtk::Stack,
    crumbs: gtk::Box,
    path: gtk::Entry,
    search: gtk::Entry,
    search_button: gtk::ToggleButton,
    recursive: Cell<bool>,
    changing: Cell<bool>,
    query: Rc<RefCell<String>>,
    hidden: Rc<Cell<bool>>,
    status: gtk::Label,
    empty: gtk::Label,
    spinner: gtk::Spinner,
    loading: Cell<bool>,
    load_cancel: RefCell<gio::Cancellable>,
    search_cancel: RefCell<gio::Cancellable>,
    monitor: RefCell<Option<gio::Cancellable>>,
    search_revision: Cell<u64>,
    completion_revision: Cell<u64>,
    pending_selection: RefCell<Vec<String>>,
    selection_scheduled: Cell<bool>,
    operations: Operations,
    thumbs: Thumbnails,
    volume_monitor: RefCell<Option<gio::VolumeMonitor>>,
}

pub fn open(app: &adw::Application, file: gio::File, selected: Vec<String>) -> Rc<FileWindow> {
    let state = FileWindow::new(app);
    WINDOWS.with(|windows| windows.borrow_mut().push(state.clone()));
    state.pending_selection.replace(selected);
    state.navigate(file, true);
    state.window.present();
    state
}
pub fn report(app: &adw::Application, message: impl AsRef<str>) {
    let window = WINDOWS.with(|windows| windows.borrow().iter().find(|w| w.app == *app).cloned());
    if let Some(window) = window {
        window.notify(message.as_ref());
    } else {
        open(app, gio::File::for_path(glib::home_dir()), Vec::new()).notify(message.as_ref());
    }
}
pub fn show_items(app: &adw::Application, uris: Vec<String>) {
    let mut folders: HashMap<String, Vec<String>> = HashMap::new();
    for uri in uris {
        let item = gio::File::for_uri(&uri);
        if let Some(parent) = item.parent() {
            folders
                .entry(parent.uri().to_string())
                .or_default()
                .push(uri);
        }
    }
    for (folder, selected) in folders {
        open(app, gio::File::for_uri(&folder), selected);
    }
}
impl FileWindow {
    fn new(app: &adw::Application) -> Rc<Self> {
        let config = Rc::new(RefCell::new(Config::default()));
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Files")
            .default_width(1120)
            .default_height(720)
            .build();
        let header = adw::HeaderBar::new();
        header.set_show_title(true);
        let back_button = icon_button("go-previous-symbolic", "Back (Alt+Left)");
        let forward_button = icon_button("go-next-symbolic", "Forward (Alt+Right)");
        let up_button = icon_button("go-up-symbolic", "Up (Alt+Up)");
        header.pack_start(&back_button);
        header.pack_start(&forward_button);
        header.pack_start(&up_button);
        let path_stack = gtk::Stack::new();
        path_stack.add_css_class("path-bar");
        path_stack.set_hexpand(true);
        let crumbs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .child(&crumbs)
            .build();
        path_stack.add_named(&scroll, Some("crumbs"));
        let path = gtk::Entry::new();
        path.set_has_frame(false);
        path.set_placeholder_text(Some("Path or file URI · Tab to complete"));
        path_stack.add_named(&path, Some("entry"));
        header.set_title_widget(Some(&path_stack));
        let search = gtk::Entry::new();
        search.set_placeholder_text(Some("Filter this folder"));
        search.set_visible(false);
        search.set_width_chars(22);
        header.pack_end(&search);
        let search_button = gtk::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Search (Ctrl+F for subfolders)")
            .build();
        header.pack_end(&search_button);
        let list_button = icon_button("view-list-symbolic", "List view (Ctrl+2)");
        let grid_button = icon_button("view-grid-symbolic", "Grid view (Ctrl+1)");
        header.pack_end(&list_button);
        header.pack_end(&grid_button);
        let menu = gio::Menu::new();
        menu.append(Some("Show Hidden Files"), Some("win.hidden"));
        menu.append(Some("New Window"), Some("win.new-window"));
        menu.append(Some("New Folder"), Some("win.new-folder"));
        menu.append(Some("Empty Trash…"), Some("win.empty-trash"));
        menu.append(Some("About Files"), Some("win.about"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .build();
        header.pack_end(&menu_button);
        let operations = Operations::new();
        header.pack_end(&operations.button);
        let spinner = gtk::Spinner::new();
        header.pack_end(&spinner);
        let store = gio::ListStore::new::<gio::FileInfo>();
        let query = Rc::new(RefCell::new(String::new()));
        let hidden = Rc::new(Cell::new(false));
        let filter = gtk::CustomFilter::new(glib::clone!(
            #[strong]
            query,
            #[strong]
            hidden,
            move |obj| {
                obj.downcast_ref::<gio::FileInfo>().is_some_and(|i| {
                    (hidden.get() || !i.is_hidden())
                        && i.display_name()
                            .to_lowercase()
                            .contains(query.borrow().as_str())
                })
            }
        ));
        let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
        filtered.set_incremental(true);
        let thumbs = Thumbnails::new();
        // The callbacks are installed after construction through a weak state slot.
        let slot: Rc<RefCell<std::rc::Weak<Self>>> = Rc::new(RefCell::new(std::rc::Weak::new()));
        let rename = Rc::new(glib::clone!(
            #[strong]
            slot,
            move |info, name| if let Some(w) = slot.borrow().upgrade() {
                w.rename(info, name);
            }
        ));
        let reporter: Report = Rc::new(glib::clone!(
            #[strong]
            slot,
            move |message: String| if let Some(w) = slot.borrow().upgrade() {
                w.notify(&message);
            }
        ));
        let views = Views::new(&filtered, thumbs.clone(), rename, reporter);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&views.stack));
        let empty = gtk::Label::new(Some("Loading…"));
        empty.set_halign(gtk::Align::Center);
        empty.set_valign(gtk::Align::Center);
        empty.add_css_class("dim-label");
        empty.set_can_target(false);
        overlay.add_overlay(&empty);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&overlay);
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        footer.add_css_class("status-footer");
        let status = gtk::Label::new(Some("Loading…"));
        status.set_hexpand(true);
        status.set_xalign(0.);
        footer.append(&status);
        let hint = gtk::Label::new(Some("Space  preview   ·   Shift+F4  terminal"));
        footer.append(&hint);
        content.append(&footer);
        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_position(196);
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);
        paned.set_end_child(Some(&content));
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.append(&header);
        layout.append(&paned);
        paned.set_vexpand(true);
        let toast = adw::ToastOverlay::new();
        toast.set_child(Some(&layout));
        window.set_content(Some(&toast));
        let this = Rc::new(Self {
            window,
            app: app.clone(),
            config,
            config_revision: Cell::new(0),
            config_saving: Cell::new(false),
            toast,
            store,
            filter,
            views,
            current: RefCell::new(gio::File::for_path(glib::home_dir())),
            back: RefCell::new(Vec::new()),
            forward: RefCell::new(Vec::new()),
            back_button,
            forward_button,
            up_button,
            path_stack,
            crumbs,
            path,
            search,
            search_button,
            recursive: Cell::new(false),
            changing: Cell::new(false),
            query,
            hidden,
            status,
            empty,
            spinner,
            loading: Cell::new(false),
            load_cancel: RefCell::new(gio::Cancellable::new()),
            search_cancel: RefCell::new(gio::Cancellable::new()),
            monitor: RefCell::new(None),
            search_revision: Cell::new(0),
            completion_revision: Cell::new(0),
            pending_selection: RefCell::new(Vec::new()),
            selection_scheduled: Cell::new(false),
            operations,
            thumbs,
            volume_monitor: RefCell::new(None),
        });
        slot.replace(Rc::downgrade(&this));
        let (side, monitor) = sidebar::build(
            &this.window,
            Rc::new(glib::clone!(
                #[weak]
                this,
                move |file| this.navigate(file, true)
            )),
            this.reporter(),
        );
        paned.set_start_child(Some(&side));
        this.volume_monitor.replace(Some(monitor));
        this.connect();
        this.actions();
        this.dnd();
        grid_button.connect_clicked(glib::clone!(
            #[weak]
            this,
            move |_| this.set_view(ViewMode::Grid)
        ));
        list_button.connect_clicked(glib::clone!(
            #[weak]
            this,
            move |_| this.set_view(ViewMode::List)
        ));
        glib::spawn_future_local(glib::clone!(
            #[weak]
            this,
            async move {
                match gio::spawn_blocking(Config::load).await {
                    Ok(Ok(config)) => {
                        this.hidden.set(config.show_hidden);
                        this.filter.changed(gtk::FilterChange::Different);
                        this.views
                            .stack
                            .set_visible_child_name(if config.view == ViewMode::Grid {
                                "grid"
                            } else {
                                "list"
                            });
                        if let Some(action) = this
                            .window
                            .lookup_action("hidden")
                            .and_downcast::<gio::SimpleAction>()
                        {
                            action.set_state(&config.show_hidden.to_variant());
                        }
                        this.config.replace(config);
                    }
                    Ok(Err(e)) => this.notify(&format!("Configuration: {e}")),
                    Err(_) => this.notify("Configuration worker failed"),
                }
            }
        ));
        this
    }
    pub fn notify(&self, message: &str) {
        self.toast.add_toast(adw::Toast::new(message));
    }
    fn error(&self, e: glib::Error) {
        if !ops::is_cancelled(&e) {
            self.notify(&e.to_string());
        }
    }
    fn reporter(self: &Rc<Self>) -> Report {
        Rc::new(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |message: String| this.notify(&message)
        ))
    }
    fn connect(self: &Rc<Self>) {
        self.window.connect_destroy(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_| {
                this.load_cancel.borrow().cancel();
                this.search_cancel.borrow().cancel();
                this.operations.cancel_all();
                if let Some(monitor) = this.monitor.borrow_mut().take() {
                    monitor.cancel();
                }
                WINDOWS.with(|windows| windows.borrow_mut().retain(|w| !Rc::ptr_eq(w, &this)));
            }
        ));
        self.back_button.connect_clicked(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_| this.history(false)
        ));
        self.forward_button.connect_clicked(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_| this.history(true)
        ));
        self.up_button.connect_clicked(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_| this.up()
        ));
        self.path.connect_activate(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |entry| {
                let value = entry.text();
                if value.contains('\0') {
                    this.notify("Paths cannot contain a null character");
                    return;
                }
                let file = this.resolve_path(&value);
                this.path_stack.set_visible_child_name("crumbs");
                this.navigate(file, true);
            }
        ));
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to=this)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    this.path_stack.set_visible_child_name("crumbs");
                    this.views.stack.grab_focus();
                    glib::Propagation::Stop
                } else if key == gdk::Key::Tab {
                    this.complete_path();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        self.path.add_controller(keys);
        self.search_button.connect_toggled(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |button| {
                if this.changing.get() {
                    return;
                }
                this.search.set_visible(button.is_active());
                if button.is_active() {
                    this.search.grab_focus();
                } else {
                    this.clear_search();
                }
            }
        ));
        self.search.connect_changed(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_| if !this.changing.get() {
                this.update_search();
            }
        ));
        self.search.connect_activate(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_| if let Some(info) =
                this.views.selection.item(0).and_downcast::<gio::FileInfo>()
            {
                this.open_item(info);
            }
        ));
        let search_keys = gtk::EventControllerKey::new();
        search_keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to=this)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    this.search_button.set_active(false);
                    this.views.stack.grab_focus();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        self.search.add_controller(search_keys);
        self.views.grid.connect_activate(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_, position| this.activate(position)
        ));
        self.views.list.connect_activate(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_, position| this.activate(position)
        ));
        self.views.selection.connect_selection_changed(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_, _, _| this.update_status()
        ));
        self.views.sorted.connect_items_changed(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_, _, _, _| {
                this.schedule_selection();
                this.update_status();
            }
        ));
        self.views.sorted.connect_pending_notify(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_| {
                this.schedule_selection();
                this.update_status();
            }
        ));
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to=this)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, state| {
                if gtk::prelude::GtkWindowExt::focus(&this.window).is_some_and(|widget| {
                    widget.is::<gtk::Entry>()
                        || widget.is::<gtk::TextView>()
                        || widget.ancestor(gtk::Entry::static_type()).is_some()
                }) {
                    return glib::Propagation::Proceed;
                }
                if state.intersects(
                    gdk::ModifierType::CONTROL_MASK
                        | gdk::ModifierType::ALT_MASK
                        | gdk::ModifierType::SUPER_MASK,
                ) {
                    return glib::Propagation::Proceed;
                }
                let handled = match key {
                    gdk::Key::space => {
                        this.quicklook();
                        true
                    }
                    gdk::Key::Return | gdk::Key::KP_Enter => {
                        this.open_selected();
                        true
                    }
                    gdk::Key::Delete => {
                        this.delete(state.contains(gdk::ModifierType::SHIFT_MASK));
                        true
                    }
                    gdk::Key::F2 => {
                        this.views.rename_selected();
                        true
                    }
                    gdk::Key::BackSpace => {
                        this.history(false);
                        true
                    }
                    gdk::Key::Escape => {
                        this.search_button.set_active(false);
                        this.views.selection.unselect_all();
                        true
                    }
                    _ => {
                        if let Some(c) = key
                            .to_unicode()
                            .filter(|c| !c.is_control() && !c.is_whitespace())
                        {
                            this.recursive.set(false);
                            this.search.set_placeholder_text(Some("Filter this folder"));
                            this.search_button.set_active(true);
                            this.search.set_text(&format!("{}{c}", this.search.text()));
                            this.search.set_position(-1);
                            true
                        } else {
                            false
                        }
                    }
                };
                if handled {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        self.window.add_controller(keys);
        // A context gesture on each view's ancestor also covers empty space.
        let click = gtk::GestureClick::new();
        click.set_button(3);
        click.connect_pressed(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |gesture, _, x, y| {
                let mut on_item = false;
                if let Some(widget) = this.views.stack.pick(x, y, gtk::PickFlags::DEFAULT) {
                    let mut item = Some(widget);
                    while let Some(widget) = item {
                        if let Some(row) = widget.downcast_ref::<gtk::Box>() {
                            if let Some(label) =
                                row.last_child().and_downcast::<gtk::EditableLabel>()
                            {
                                on_item = true;
                                let uri = label.widget_name();
                                for position in 0..this.views.selection.n_items() {
                                    if this
                                        .views
                                        .selection
                                        .item(position)
                                        .and_downcast::<gio::FileInfo>()
                                        .is_some_and(|i| model::file(&i).uri() == uri)
                                    {
                                        if !this.views.selection.is_selected(position) {
                                            this.views.selection.select_item(position, true);
                                        }
                                        break;
                                    }
                                }
                                break;
                            }
                        }
                        item = widget.parent();
                    }
                }
                if !on_item {
                    this.views.selection.unselect_all();
                }
                this.context_menu(x, y);
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
        ));
        self.views.stack.add_controller(click);
    }
    fn action(self: &Rc<Self>, name: &str, keys: &[&str], run: impl Fn(&Rc<Self>) + 'static) {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |_, _| run(&this)
        ));
        self.window.add_action(&action);
        if !keys.is_empty() {
            self.app.set_accels_for_action(&format!("win.{name}"), keys);
        }
    }
    fn actions(self: &Rc<Self>) {
        self.action("back", &["<Alt>Left"], |w| w.history(false));
        self.action("forward", &["<Alt>Right"], |w| w.history(true));
        self.action("up", &["<Alt>Up"], |w| w.up());
        self.action("path", &["<Control>l"], |w| {
            w.path.set_text(&w.current.borrow().parse_name());
            w.path_stack.set_visible_child_name("entry");
            w.path.grab_focus();
            w.path.select_region(0, -1);
        });
        self.action("search", &["<Control>f"], |w| {
            w.recursive.set(true);
            w.search
                .set_placeholder_text(Some("Search subfolders · 10,000 results max"));
            w.search_button.set_active(true);
            w.search.grab_focus();
            w.update_search();
        });
        self.action("grid", &["<Control>1"], |w| w.set_view(ViewMode::Grid));
        self.action("list", &["<Control>2"], |w| w.set_view(ViewMode::List));
        let hidden = gio::SimpleAction::new_stateful("hidden", None, &false.to_variant());
        hidden.connect_activate(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move |action, _| {
                let hidden = !this.hidden.get();
                this.hidden.set(hidden);
                action.set_state(&hidden.to_variant());
                this.filter.changed(gtk::FilterChange::Different);
                this.config.borrow_mut().show_hidden = hidden;
                this.save_config();
                if this.recursive.get() {
                    this.update_search();
                }
            }
        ));
        self.window.add_action(&hidden);
        self.app
            .set_accels_for_action("win.hidden", &["<Control>h"]);
        self.action("new-window", &["<Control>n"], |w| {
            open(&w.app, w.current.borrow().clone(), Vec::new());
        });
        self.action("new-folder", &["<Control><Shift>n"], |w| w.new_folder());
        self.action("copy", &["<Control>c"], |w| w.copy(false));
        self.action("cut", &["<Control>x"], |w| w.copy(true));
        self.action("paste", &["<Control>v"], |w| w.paste());
        self.action("terminal", &["<Shift>F4"], |w| w.terminal());
        self.action("open", &[], |w| w.open_selected());
        self.action("open-with", &[], |w| w.open_with());
        self.action("rename", &[], |w| w.views.rename_selected());
        self.action("trash", &[], |w| w.delete(false));
        self.action("delete-permanently", &[], |w| w.delete(true));
        self.action("properties", &["<Alt>Return"], |w| {
            if let Some(info) = w.views.selected().first() {
                preview::properties(&w.window, info, w.reporter());
            }
        });
        self.action("restore", &[], |w| w.restore_selected());
        self.action("empty-trash", &[], |w| w.empty_trash());
        self.action("select-all", &["<Control>a"], |w| {
            w.views.selection.select_all();
        });
        self.action("refresh", &["F5", "<Control>r"], |w| w.reload());
        self.action("about", &[], |w| {
            let dialog = adw::AboutDialog::builder()
                .application_name("Files")
                .application_icon("system-file-manager")
                .developer_name("GAVNO")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("A file manager for the GAVNO desktop")
                .license_type(gtk::License::MitX11)
                .build();
            dialog.present(Some(&w.window));
        });
    }
    fn set_view(self: &Rc<Self>, view: ViewMode) {
        self.views
            .stack
            .set_visible_child_name(if view == ViewMode::Grid {
                "grid"
            } else {
                "list"
            });
        self.config.borrow_mut().view = view;
        self.save_config();
    }
    fn save_config(self: &Rc<Self>) {
        self.config_revision.set(self.config_revision.get() + 1);
        if self.config_saving.replace(true) {
            return;
        }
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to=this)]
            self,
            async move {
                loop {
                    let revision = this.config_revision.get();
                    let config = this.config.borrow().clone();
                    match gio::spawn_blocking(move || config.save()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => this.notify(&e),
                        Err(_) => this.notify("Configuration worker failed"),
                    }
                    if this.config_revision.get() == revision {
                        break;
                    }
                }
                this.config_saving.set(false);
            }
        ));
    }
    fn history(self: &Rc<Self>, forward: bool) {
        let file = if forward {
            self.forward.borrow_mut().pop()
        } else {
            self.back.borrow_mut().pop()
        };
        if let Some(file) = file {
            if forward {
                self.back.borrow_mut().push(self.current.borrow().clone());
            } else {
                self.forward
                    .borrow_mut()
                    .push(self.current.borrow().clone());
            }
            self.navigate(file, false);
        }
    }
    fn up(self: &Rc<Self>) {
        let parent = self.current.borrow().parent();
        if let Some(parent) = parent {
            self.navigate(parent, true);
        }
    }
    fn resolve_path(&self, text: &str) -> gio::File {
        let text = text.trim();
        if text == "~" {
            return gio::File::for_path(glib::home_dir());
        }
        if let Some(tail) = text.strip_prefix("~/") {
            return gio::File::for_path(glib::home_dir().join(tail));
        }
        if text.contains("://") {
            gio::File::for_uri(text)
        } else if text.starts_with('/') {
            gio::File::for_path(text)
        } else {
            self.current.borrow().resolve_relative_path(text)
        }
    }
    pub fn navigate(self: &Rc<Self>, file: gio::File, history: bool) {
        if !matches!(file.uri_scheme().as_deref(), Some("file" | "trash")) {
            self.notify("Network locations are not supported in v1");
            return;
        }
        if history && file != *self.current.borrow() {
            self.back.borrow_mut().push(self.current.borrow().clone());
            self.forward.borrow_mut().clear();
        }
        self.current.replace(file.clone());
        self.search_cancel.borrow().cancel();
        self.recursive.set(false);
        self.changing.set(true);
        self.search.set_text("");
        self.query.borrow_mut().clear();
        self.search_button.set_active(false);
        self.search.set_visible(false);
        self.search.set_placeholder_text(Some("Filter this folder"));
        self.changing.set(false);
        self.filter.changed(gtk::FilterChange::Different);
        self.render_path();
        self.load(file);
    }
    fn render_path(self: &Rc<Self>) {
        self.back_button
            .set_sensitive(!self.back.borrow().is_empty());
        self.forward_button
            .set_sensitive(!self.forward.borrow().is_empty());
        self.up_button
            .set_sensitive(self.current.borrow().parent().is_some());
        while let Some(child) = self.crumbs.first_child() {
            self.crumbs.remove(&child);
        }
        let file = self.current.borrow();
        self.window.set_title(Some(&format!(
            "{} — Files",
            file.basename()
                .map(|p| p.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "System".into())
        )));
        if let Some(path) = file.path() {
            for (label, path) in model::breadcrumbs(&path, &glib::home_dir()) {
                let button = gtk::Button::with_label(&label);
                button.add_css_class("flat");
                button.connect_clicked(glib::clone!(
                    #[weak(rename_to=this)]
                    self,
                    move |_| this.navigate(gio::File::for_path(&path), true)
                ));
                self.crumbs.append(&button);
                self.crumbs.append(&gtk::Label::new(Some("›")));
            }
        } else {
            self.crumbs.append(&gtk::Label::new(Some("Trash")));
        }
        self.path_stack.set_visible_child_name("crumbs");
    }
    fn reload(self: &Rc<Self>) {
        if self.recursive.get() {
            self.update_search();
        } else {
            if self.pending_selection.borrow().is_empty() {
                self.pending_selection.replace(
                    self.views
                        .selected()
                        .iter()
                        .map(|i| model::file(i).uri().to_string())
                        .collect(),
                );
            }
            let file = self.current.borrow().clone();
            self.load(file);
        }
    }
    fn load(self: &Rc<Self>, file: gio::File) {
        self.load_cancel.borrow().cancel();
        let cancel = gio::Cancellable::new();
        self.load_cancel.replace(cancel.clone());
        self.store.remove_all();
        self.loading.set(true);
        self.spinner.start();
        self.update_status();
        if let Some(monitor) = self.monitor.borrow_mut().take() {
            monitor.cancel();
        }
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to=this)]
            self,
            async move {
                let result = async {
                    let e = ops::enumerate_display(&file, model::ATTRIBUTES, &cancel).await?;
                    loop {
                        let batch = ops::next(&e, &cancel).await?;
                        if batch.is_empty() {
                            break;
                        }
                        cancel.set_error_if_cancelled()?;
                        for info in &batch {
                            model::attach(info, &e.child(info));
                        }
                        this.store.extend_from_slice(&batch);
                        glib::timeout_future(std::time::Duration::from_millis(1)).await;
                    }
                    e.close_future(glib::Priority::DEFAULT).await?;
                    Ok::<_, glib::Error>(())
                }
                .await;
                if cancel.is_cancelled() {
                    return;
                }
                this.loading.set(false);
                this.spinner.stop();
                this.update_status();
                this.schedule_selection();
                if let Err(e) = result {
                    this.empty.set_text("Unable to open this folder");
                    this.error(e);
                } else {
                    this.watch(file, cancel);
                }
            }
        ));
    }
    fn watch(self: &Rc<Self>, file: gio::File, cancel: gio::Cancellable) {
        if cancel.is_cancelled() {
            return;
        }
        let watch_cancel = gio::Cancellable::new();
        let receiver = model::watch_directory(file, watch_cancel.clone());
        self.monitor.replace(Some(watch_cancel.clone()));
        let mut dirty = false;
        glib::timeout_add_local(
            std::time::Duration::from_millis(160),
            glib::clone!(
                #[weak(rename_to=this)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if watch_cancel.is_cancelled() {
                        return glib::ControlFlow::Break;
                    }
                    while let Ok(event) = receiver.try_recv() {
                        match event {
                            Ok(()) => dirty = true,
                            Err(e) => this.notify(&e),
                        }
                    }
                    if dirty && !this.loading.get() && !this.recursive.get() {
                        this.reload();
                        return glib::ControlFlow::Break;
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );
    }
    fn schedule_selection(self: &Rc<Self>) {
        if self.selection_scheduled.replace(true) {
            return;
        }
        glib::idle_add_local_once(glib::clone!(
            #[weak(rename_to=this)]
            self,
            move || {
                this.selection_scheduled.set(false);
                this.select_pending();
            }
        ));
    }
    fn select_pending(&self) {
        // Never mutate the selection from inside ListModel::items-changed: the
        // views must finish consuming the model notification first.
        if self.loading.get() || self.views.sorted.pending() != 0 {
            return;
        }
        let mut pending = self.pending_selection.borrow_mut();
        if pending.is_empty() {
            return;
        }
        pending.retain(|uri| {
            for i in 0..self.views.selection.n_items() {
                if self
                    .views
                    .selection
                    .item(i)
                    .and_downcast::<gio::FileInfo>()
                    .is_some_and(|info| model::file(&info).uri().as_str() == uri)
                {
                    self.views.selection.select_item(i, false);
                    if self.views.stack.visible_child_name().as_deref() == Some("list") {
                        self.views
                            .list
                            .scroll_to(i, None, gtk::ListScrollFlags::NONE, None);
                    } else {
                        self.views
                            .grid
                            .scroll_to(i, gtk::ListScrollFlags::NONE, None);
                    }
                    return false;
                }
            }
            true
        });
    }
    fn update_status(&self) {
        let count = self.views.selection.n_items();
        let selected = self.views.selection.selection().size();
        self.status.set_text(&if selected > 0 {
            let size: u64 = self
                .views
                .selected()
                .iter()
                .filter(|i| !model::is_dir(i))
                .map(|i| i.size().max(0) as u64)
                .sum();
            format!(
                "{selected} of {count} selected · {}",
                model::format_size(size)
            )
        } else {
            format!(
                "{count} items{}",
                if self.loading.get() {
                    " · Loading…"
                } else {
                    ""
                }
            )
        });
        self.empty.set_visible(count == 0);
        self.empty.set_text(if self.loading.get() {
            "Loading…"
        } else if !self.query.borrow().is_empty() {
            "Nothing found"
        } else if self.is_trash() {
            "Trash is empty"
        } else {
            "This folder is empty"
        });
    }
    fn activate(self: &Rc<Self>, position: u32) {
        if let Some(info) = self
            .views
            .selection
            .item(position)
            .and_downcast::<gio::FileInfo>()
        {
            self.open_item(info);
        }
    }
    fn open_selected(self: &Rc<Self>) {
        for info in self.views.selected() {
            self.open_item(info);
        }
    }
    fn open_item(self: &Rc<Self>, info: gio::FileInfo) {
        if self.is_trash() {
            self.quicklook();
            return;
        }
        if model::is_dir(&info) {
            self.navigate(model::file(&info), true);
        } else {
            let uri = model::file(&info).uri();
            let context = gtk::prelude::WidgetExt::display(&self.window).app_launch_context();
            glib::spawn_future_local(glib::clone!(
                #[weak(rename_to=this)]
                self,
                async move {
                    if let Err(e) =
                        gio::AppInfo::launch_default_for_uri_future(&uri, Some(&context)).await
                    {
                        this.error(e);
                    }
                }
            ));
        }
    }
    fn open_with(self: &Rc<Self>) {
        if let Some(info) = self.views.selected().first() {
            let launcher = gtk::FileLauncher::new(Some(&model::file(info)));
            launcher.set_always_ask(true);
            glib::spawn_future_local(glib::clone!(
                #[weak(rename_to=this)]
                self,
                async move {
                    if let Err(e) = launcher.launch_future(Some(&this.window)).await {
                        if !e.matches(gtk::DialogError::Dismissed) {
                            this.error(e);
                        }
                    }
                }
            ));
        }
    }
    fn quicklook(self: &Rc<Self>) {
        if let Some(info) = self.views.selected().first() {
            preview::show(&self.window, info, self.thumbs.clone(), self.reporter());
        }
    }
    fn is_trash(&self) -> bool {
        self.current.borrow().uri_scheme().as_deref() == Some("trash")
    }
    fn rename(self: &Rc<Self>, info: gio::FileInfo, name: String) {
        if self.is_trash() {
            self.notify("Restore files before renaming them");
            return;
        }
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            self.notify("Choose a nonempty name without a slash");
            self.reload();
            return;
        }
        let file = model::file(&info);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to=this)]
            self,
            async move {
                match gio::spawn_blocking(move || {
                    file.set_display_name(&name, gio::Cancellable::NONE)
                })
                .await
                {
                    Ok(Ok(file)) => {
                        this.pending_selection.replace(vec![file.uri().to_string()]);
                        this.reload();
                    }
                    Ok(Err(e)) => this.error(e),
                    Err(_) => this.notify("Rename worker failed"),
                }
            }
        ));
    }
    fn new_folder(self: &Rc<Self>) {
        if self.is_trash() {
            self.notify("New folders cannot be created in Trash");
            return;
        }
        let dialog = adw::AlertDialog::builder().heading("New Folder").build();
        let name = gtk::Entry::builder()
            .text("New Folder")
            .activates_default(true)
            .build();
        dialog.set_extra_child(Some(&name));
        dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        let folder = self.current.borrow().clone();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to=this)]
            self,
            async move {
                if dialog.choose_future(Some(&this.window)).await != "create" {
                    return;
                }
                let name = name.text();
                if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
                    this.notify("Invalid folder name");
                    return;
                }
                let target = folder.child(name.as_str());
                let dest = target.clone();
                match gio::spawn_blocking(move || dest.make_directory(gio::Cancellable::NONE)).await
                {
                    Ok(Ok(())) => {
                        this.pending_selection
                            .replace(vec![target.uri().to_string()]);
                        this.reload();
                    }
                    Ok(Err(e)) => this.error(e),
                    Err(_) => this.notify("Folder worker failed"),
                }
            }
        ));
    }
    fn copy(&self, cut: bool) {
        if self.is_trash() {
            self.notify("Restore files before copying or cutting them");
            return;
        }
        let uris = self
            .views
            .selected()
            .iter()
            .map(|i| model::file(i).uri().to_string())
            .collect::<Vec<_>>();
        if uris.is_empty() {
            return;
        }
        let files = clipboard::Files { cut, uris };
        if let Err(e) = self.window.clipboard().set_content(Some(&files.provider())) {
            self.notify(&e.to_string());
        } else {
            self.notify(if cut { "Files cut" } else { "Files copied" });
        }
    }
    fn paste(self: &Rc<Self>) {
        if self.is_trash() {
            self.notify("Files cannot be pasted into Trash");
            return;
        }
        let clipboard = self.window.clipboard();
        let destination = self.current.borrow().clone();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to=this)]
            self,
            async move {
                match clipboard::read(&clipboard).await {
                    Ok(files) => this.transfer(files, destination, true, None),
                    Err(e) => this.notify(&e),
                }
            }
        ));
    }
    fn transfer(
        self: &Rc<Self>,
        files: clipboard::Files,
        destination: gio::File,
        clear_clipboard: bool,
        drop: Option<gdk::Drop>,
    ) {
        let op = self.operations.start(if files.cut {
            "Moving files"
        } else {
            "Copying files"
        });
        let sources = files
            .uris
            .iter()
            .map(|uri| gio::File::for_uri(uri))
            .collect();
        let _hold = self.app.hold();
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to=this)]
            self,
            async move {
                let _hold = _hold;
                let mut success = false;
                match ops::transfer(&this.window, sources, destination, files.cut, &op).await {
                    Ok(all) => {
                        success = all;
                        if all && files.cut && clear_clipboard {
                            let clipboard = this.window.clipboard();
                            if clipboard::read(&clipboard)
                                .await
                                .is_ok_and(|current| current == files)
                            {
                                if let Err(e) = clipboard.set_content(None::<&gdk::ContentProvider>)
                                {
                                    this.notify(&e.to_string());
                                }
                            }
                        }
                        this.notify(&format!("{} items completed", op.completed.get()));
                    }
                    Err(e) => this.error(e),
                }
                if let Some(drop) = drop {
                    drop.finish(if success {
                        if files.cut {
                            gdk::DragAction::MOVE
                        } else {
                            gdk::DragAction::COPY
                        }
                    } else {
                        gdk::DragAction::empty()
                    });
                }
                op.finish();
                this.reload();
            }
        ));
    }
    fn delete(self: &Rc<Self>, permanent: bool) {
        let selected = self.views.selected();
        if selected.is_empty() {
            return;
        }
        let permanent = permanent || self.is_trash();
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to=this)]
            self,
            async move {
                if permanent
                    && !ops::confirm(
                        &this.window,
                        "Delete permanently?",
                        &format!(
                            "{} selected items will be permanently deleted. This cannot be undone.",
                            selected.len()
                        ),
                        "Delete Permanently",
                    )
                    .await
                {
                    return;
                }
                let _hold = this.app.hold();
                let op = this.operations.start(if permanent {
                    "Deleting files"
                } else {
                    "Moving to Trash"
                });
                let trash = gio::File::for_uri("trash:///");
                let before = if !permanent {
                    match ops::children(&trash, &op.cancel).await {
                        Ok(items) => Some(
                            items
                                .iter()
                                .map(|i| model::file(i).uri().to_string())
                                .collect::<HashSet<_>>(),
                        ),
                        Err(e) => {
                            this.error(e);
                            None
                        }
                    }
                } else {
                    None
                };
                let mut originals = HashSet::new();
                for info in selected {
                    let file = model::file(&info);
                    op.progress(&info.display_name(), 0, 0);
                    let result = if permanent {
                        ops::delete_tree(&file, &op.cancel).await
                    } else {
                        ops::trash(&file, &op.cancel).await
                    };
                    match result {
                        Ok(()) => {
                            op.completed.set(op.completed.get() + 1);
                            if let Some(path) = file.path() {
                                originals.insert(path.to_string_lossy().into_owned());
                            }
                        }
                        Err(e) => {
                            let cancelled = ops::is_cancelled(&e);
                            this.error(e);
                            if cancelled {
                                break;
                            }
                        }
                    }
                }
                if let Some(before) = before {
                    match ops::children(&trash, &gio::Cancellable::new()).await {
                        Ok(items) => {
                            let undo = items
                                .into_iter()
                                .filter(|i| {
                                    !before.contains(model::file(i).uri().as_str())
                                        && i.attribute_byte_string("trash::orig-path")
                                            .is_some_and(|p| originals.contains(p.as_str()))
                                })
                                .collect::<Vec<_>>();
                            if !undo.is_empty() {
                                let toast = adw::Toast::builder()
                                    .title(format!("{} items moved to Trash", undo.len()))
                                    .button_label("Undo")
                                    .timeout(12)
                                    .build();
                                toast.connect_button_clicked(glib::clone!(
                                    #[weak]
                                    this,
                                    move |_| this.restore(undo.clone())
                                ));
                                this.toast.add_toast(toast);
                            }
                        }
                        Err(e) => this.error(e),
                    }
                } else if op.completed.get() > 0 {
                    this.notify(&format!("{} items completed", op.completed.get()));
                }
                op.finish();
                this.reload();
            }
        ));
    }
    fn restore_selected(self: &Rc<Self>) {
        self.restore(self.views.selected());
    }
    fn restore(self: &Rc<Self>, items: Vec<gio::FileInfo>) {
        let op = self.operations.start("Restoring files");
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to=this)]
            self,
            async move {
                let _hold = this.app.hold();
                for info in items {
                    op.progress(&info.display_name(), 0, 0);
                    match ops::restore(&this.window, &info, &op.cancel).await {
                        Ok(()) => op.completed.set(op.completed.get() + 1),
                        Err(e) => {
                            let cancelled = ops::is_cancelled(&e);
                            this.error(e);
                            if cancelled {
                                break;
                            }
                        }
                    }
                }
                op.finish();
                this.reload();
            }
        ));
    }
    fn empty_trash(self: &Rc<Self>) {
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to=this)]
            self,
            async move {
                if !ops::confirm(
                    &this.window,
                    "Empty Trash?",
                    "All items in Trash will be permanently deleted. This cannot be undone.",
                    "Empty Trash",
                )
                .await
                {
                    return;
                }
                let _hold = this.app.hold();
                let op = this.operations.start("Emptying Trash");
                match ops::children(&gio::File::for_uri("trash:///"), &op.cancel).await {
                    Ok(items) => {
                        for info in items {
                            op.progress(&info.display_name(), 0, 0);
                            match ops::delete_tree(&model::file(&info), &op.cancel).await {
                                Ok(()) => {}
                                Err(e) => {
                                    let cancelled = ops::is_cancelled(&e);
                                    this.error(e);
                                    if cancelled {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => this.error(e),
                }
                op.finish();
                this.reload();
            }
        ));
    }
    fn terminal(self: &Rc<Self>) {
        let selected = self.views.selected();
        let folder = selected
            .first()
            .filter(|i| model::is_dir(i))
            .map(model::file)
            .unwrap_or_else(|| self.current.borrow().clone());
        let Some(path) = folder.path() else {
            self.notify("Open in Terminal requires a local folder");
            return;
        };
        let command = self.config.borrow().terminal_command();
        if command.contains('\0') {
            self.notify("The terminal command contains a null character");
            return;
        }
        let command = match glib::shell_parse_argv(command) {
            Ok(command) => command,
            Err(e) => {
                self.error(e);
                return;
            }
        };
        if command.is_empty() {
            self.notify("The terminal command is empty");
            return;
        }
        let mut args = command
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mut placeholder = false;
        for arg in &mut args {
            if arg.contains("{dir}") {
                *arg = arg.replace("{dir}", &path.to_string_lossy());
                placeholder = true;
            }
        }
        if !placeholder {
            args.push(format!("--working-directory={}", path.display()));
        }
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to=this)]
            self,
            async move {
                match gio::spawn_blocking(move || {
                    std::process::Command::new(&args[0])
                        .args(&args[1..])
                        .spawn()
                        .map(|mut child| {
                            std::thread::spawn(move || {
                                let _ = child.wait();
                            });
                        })
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => this.notify(&e.to_string()),
                    Err(_) => this.notify("Terminal worker failed"),
                }
            }
        ));
    }
    fn clear_search(self: &Rc<Self>) {
        let was_recursive = self.recursive.replace(false);
        self.search_cancel.borrow().cancel();
        self.search_revision.set(self.search_revision.get() + 1);
        self.changing.set(true);
        self.search.set_text("");
        self.changing.set(false);
        self.query.borrow_mut().clear();
        self.filter.changed(gtk::FilterChange::Different);
        if was_recursive {
            let file = self.current.borrow().clone();
            self.load(file);
        }
    }
    fn update_search(self: &Rc<Self>) {
        let query = self.search.text().to_lowercase();
        self.query.replace(query.clone());
        self.filter.changed(gtk::FilterChange::Different);
        if !self.recursive.get() {
            return;
        }
        self.search_cancel.borrow().cancel();
        self.load_cancel.borrow().cancel();
        let cancel = gio::Cancellable::new();
        self.search_cancel.replace(cancel.clone());
        let revision = self.search_revision.get() + 1;
        self.search_revision.set(revision);
        if query.is_empty() {
            self.store.remove_all();
            self.loading.set(false);
            self.spinner.stop();
            self.update_status();
            return;
        }
        let root = self.current.borrow().clone();
        let hidden = self.hidden.get();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to=this)]
            self,
            async move {
                glib::timeout_future(std::time::Duration::from_millis(180)).await;
                if cancel.is_cancelled() || this.search_revision.get() != revision {
                    return;
                }
                this.store.remove_all();
                this.loading.set(true);
                this.spinner.start();
                this.update_status();
                let result = search::recursive(
                    root,
                    query,
                    hidden,
                    cancel.clone(),
                    glib::clone!(
                        #[weak]
                        this,
                        move |batch| {
                            this.store.extend_from_slice(&batch);
                        }
                    ),
                    glib::clone!(
                        #[weak]
                        this,
                        move |e| this.notify(&e)
                    ),
                )
                .await;
                if cancel.is_cancelled() {
                    return;
                }
                this.loading.set(false);
                this.spinner.stop();
                this.update_status();
                match result {
                    Ok(true) => this.notify("Search stopped at 10,000 results. Refine your query."),
                    Ok(false) => {}
                    Err(e) => this.error(e),
                }
            }
        ));
    }
    fn complete_path(self: &Rc<Self>) {
        let text = self.path.text().to_string();
        let (base, prefix) = match text.rsplit_once('/') {
            Some((base, prefix)) => (
                if base.is_empty() { "/" } else { base }.to_string(),
                prefix.to_string(),
            ),
            None => (".".to_string(), text.clone()),
        };
        let folder = self.resolve_path(&base);
        let revision = self.completion_revision.get() + 1;
        self.completion_revision.set(revision);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to=this)]
            self,
            async move {
                let cancel = gio::Cancellable::new();
                match ops::children(&folder, &cancel).await {
                    Ok(items) => {
                        if this.completion_revision.get() != revision || this.path.text() != text {
                            return;
                        }
                        let mut names = items
                            .into_iter()
                            .filter(|i| model::is_dir(i) && i.display_name().starts_with(&prefix))
                            .map(|i| i.display_name().to_string())
                            .collect::<Vec<_>>();
                        names.sort_by(|a, b| model::natural_cmp(a, b));
                        if names.len() == 1 {
                            let file = folder.child(&names[0]);
                            this.path
                                .set_text(&format!("{}/", file.parse_name().trim_end_matches('/')));
                            this.path.set_position(-1);
                        } else if !names.is_empty() {
                            let common =
                                names.iter().skip(1).fold(names[0].clone(), |acc, name| {
                                    acc.chars()
                                        .zip(name.chars())
                                        .take_while(|(a, b)| a == b)
                                        .map(|(a, _)| a)
                                        .collect()
                                });
                            if common.len() > prefix.len() {
                                let file = folder.child(common);
                                this.path.set_text(&file.parse_name());
                                this.path.set_position(-1);
                            }
                            let popover = gtk::Popover::new();
                            let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
                            for name in names.iter().take(40) {
                                let button = gtk::Button::with_label(name);
                                button.add_css_class("flat");
                                let file = folder.child(name);
                                button.connect_clicked(glib::clone!(
                                    #[weak]
                                    this,
                                    #[weak]
                                    popover,
                                    move |_| {
                                        this.path.set_text(&format!(
                                            "{}/",
                                            file.parse_name().trim_end_matches('/')
                                        ));
                                        this.path.set_position(-1);
                                        popover.popdown();
                                    }
                                ));
                                list.append(&button);
                            }
                            let scroll = gtk::ScrolledWindow::builder()
                                .max_content_height(320)
                                .propagate_natural_height(true)
                                .child(&list)
                                .build();
                            popover.set_child(Some(&scroll));
                            popover.set_parent(&this.path);
                            popover.connect_closed(|p| p.unparent());
                            popover.popup();
                        }
                    }
                    Err(e) => this.error(e),
                }
            }
        ));
    }
    fn context_menu(&self, x: f64, y: f64) {
        let menu = gio::Menu::new();
        let selected = self.views.selected();
        if self.is_trash() {
            if !selected.is_empty() {
                menu.append(Some("Restore"), Some("win.restore"));
                menu.append(Some("Delete Permanently…"), Some("win.delete-permanently"));
            }
            menu.append(Some("Empty Trash…"), Some("win.empty-trash"));
        } else {
            if !selected.is_empty() {
                menu.append(Some("Open"), Some("win.open"));
                if selected.len() == 1 && !model::is_dir(&selected[0]) {
                    menu.append(Some("Open With…"), Some("win.open-with"));
                }
            }
            if selected.is_empty() || selected.first().is_some_and(model::is_dir) {
                menu.append(Some("Open in Terminal"), Some("win.terminal"));
            }
            let edit = gio::Menu::new();
            if !selected.is_empty() {
                edit.append(Some("Cut"), Some("win.cut"));
                edit.append(Some("Copy"), Some("win.copy"));
            }
            edit.append(Some("Paste"), Some("win.paste"));
            if selected.len() == 1 {
                edit.append(Some("Rename"), Some("win.rename"));
            }
            if !selected.is_empty() {
                edit.append(Some("Move to Trash"), Some("win.trash"));
            } else {
                edit.append(Some("New Folder…"), Some("win.new-folder"));
            }
            menu.append_section(None, &edit);
        }
        if selected.len() == 1 {
            menu.append(Some("Properties"), Some("win.properties"));
        }
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&self.views.stack);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|p| p.unparent());
        popover.popup();
    }
    fn dnd(self: &Rc<Self>) {
        let source = gtk::DragSource::new();
        source.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);
        source.connect_prepare(glib::clone!(
            #[weak(rename_to=this)]
            self,
            #[upgrade_or]
            None,
            move |_, _, _| {
                if this.is_trash() {
                    return None;
                }
                let uris = this
                    .views
                    .selected()
                    .iter()
                    .map(|i| model::file(i).uri().to_string())
                    .collect::<Vec<_>>();
                if uris.is_empty() {
                    None
                } else {
                    Some(clipboard::Files { cut: false, uris }.provider())
                }
            }
        ));
        self.views.stack.add_controller(source);
        let target = gtk::DropTargetAsync::new(
            Some(gdk::ContentFormats::new(&[clipboard::URI_LIST])),
            gdk::DragAction::COPY | gdk::DragAction::MOVE,
        );
        target.connect_drop(glib::clone!(
            #[weak(rename_to=this)]
            self,
            #[upgrade_or]
            false,
            move |controller, drop, x, y| {
                if this.is_trash() {
                    return false;
                }
                let mut destination = this.current.borrow().clone();
                if let Some(widget) = this.views.stack.pick(x, y, gtk::PickFlags::DEFAULT) {
                    let mut current = Some(widget);
                    while let Some(widget) = current {
                        if let Some(row) = widget.downcast_ref::<gtk::Box>() {
                            if let Some(label) =
                                row.last_child().and_downcast::<gtk::EditableLabel>()
                            {
                                let uri = label.widget_name();
                                for index in 0..this.views.selection.n_items() {
                                    if let Some(info) = this
                                        .views
                                        .selection
                                        .item(index)
                                        .and_downcast::<gio::FileInfo>()
                                    {
                                        if model::file(&info).uri() == uri && model::is_dir(&info) {
                                            destination = model::file(&info);
                                            break;
                                        }
                                    }
                                }
                                break;
                            }
                        }
                        current = widget.parent();
                    }
                }
                let state = controller.current_event_state();
                let moving = state.contains(gdk::ModifierType::SHIFT_MASK)
                    || (!state.contains(gdk::ModifierType::CONTROL_MASK)
                        && drop
                            .drag()
                            .is_some_and(|d| d.selected_action() == gdk::DragAction::MOVE));
                let drop = drop.clone();
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    this,
                    async move {
                        let result = async {
                            let (stream, _) = drop
                                .read_future(&[clipboard::URI_LIST], glib::Priority::DEFAULT)
                                .await
                                .map_err(|e| e.to_string())?;
                            let mut bytes = Vec::new();
                            loop {
                                let chunk = stream
                                    .read_bytes_future(8192, glib::Priority::DEFAULT)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                if chunk.is_empty() {
                                    break;
                                }
                                bytes.extend_from_slice(&chunk);
                                if bytes.len() > 8 * 1024 * 1024 {
                                    return Err("Dropped URI list is too large".into());
                                }
                            }
                            stream
                                .close_future(glib::Priority::DEFAULT)
                                .await
                                .map_err(|e| e.to_string())?;
                            let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
                            clipboard::Files::parse(&text, false)
                        }
                        .await;
                        match result {
                            Ok(mut files) => {
                                files.cut = moving;
                                this.transfer(files, destination, false, Some(drop));
                            }
                            Err(e) => {
                                this.notify(&e);
                                drop.finish(gdk::DragAction::empty());
                            }
                        }
                    }
                ));
                true
            }
        ));
        self.views.stack.add_controller(target);
    }
}
fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("flat");
    button
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };

    async fn until(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !condition() {
            assert!(Instant::now() < deadline, "Timed out waiting for GTK/GIO");
            glib::timeout_future(Duration::from_millis(10)).await;
        }
    }
    async fn settled(state: &FileWindow) {
        until(|| !state.loading.get() && state.views.sorted.pending() == 0).await;
    }
    fn info_named(state: &FileWindow, name: &str) -> gio::FileInfo {
        (0..state.views.selection.n_items())
            .filter_map(|i| {
                state
                    .views
                    .selection
                    .item(i)
                    .and_downcast::<gio::FileInfo>()
            })
            .find(|i| i.display_name() == name)
            .expect("file in model")
    }

    #[test]
    #[ignore = "Requires a Wayland display; creates and removes an isolated 20,000-file fixture"]
    fn gtk_large_folder_and_actions() {
        let root = std::env::temp_dir().join(format!(
            "gavno-files-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _fixture = Fixture(root.clone());
        std::fs::create_dir_all(root.join("source/nested")).expect("fixture");
        std::fs::create_dir_all(root.join("destination")).expect("fixture");
        std::fs::write(
            root.join("source/nested/nested-needle.txt"),
            "preview contents",
        )
        .expect("fixture");
        std::fs::write(root.join(".hidden"), "hidden").expect("fixture");
        for n in 0..20000 {
            std::fs::write(root.join(format!("file{n}.txt")), "content").expect("fixture");
        }
        adw::init().expect("GTK display");
        let app = adw::Application::builder()
            .application_id("org.gavno.Files.Smoke")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(gio::Cancellable::NONE)
            .expect("register application");
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("style.css"));
        gtk::style_context_add_provider_for_display(
            &gtk::gdk::Display::default().expect("display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let context = glib::MainContext::default();
        context.block_on(async {
            let ticks = Rc::new(Cell::new(0u64));
            let max_gap = Rc::new(Cell::new(Duration::ZERO));
            let mut last = Instant::now();
            let heartbeat = glib::timeout_add_local(
                Duration::from_millis(10),
                glib::clone!(
                    #[strong]
                    ticks,
                    #[strong]
                    max_gap,
                    move || {
                        let now = Instant::now();
                        max_gap.set(max_gap.get().max(now.duration_since(last)));
                        last = now;
                        ticks.set(ticks.get() + 1);
                        glib::ControlFlow::Continue
                    }
                ),
            );
            let started = Instant::now();
            let state = open(&app, gio::File::for_path(&root), Vec::new());
            settled(&state).await;
            assert_eq!(state.views.selection.n_items(), 20002);
            assert!(
                ticks.get() > 5,
                "Main loop heartbeat ran during progressive loading"
            );
            assert!(
                max_gap.get() < Duration::from_secs(1),
                "Main loop stalled: {:?}",
                max_gap.get()
            );
            eprintln!(
                "20,000-file folder: {:?}; heartbeats: {}; maximum gap: {:?}",
                started.elapsed(),
                ticks.get(),
                max_gap.get()
            );
            heartbeat.remove();
            assert!(model::is_dir(
                &state
                    .views
                    .selection
                    .item(0)
                    .and_downcast::<gio::FileInfo>()
                    .expect("first item")
            ));
            state.set_view(ViewMode::List);
            let name = state
                .views
                .list
                .columns()
                .item(0)
                .and_downcast::<gtk::ColumnViewColumn>()
                .expect("name column");
            state
                .views
                .list
                .sort_by_column(Some(&name), gtk::SortType::Descending);
            settled(&state).await;
            assert!(model::is_dir(
                &state
                    .views
                    .selection
                    .item(0)
                    .and_downcast::<gio::FileInfo>()
                    .expect("folder first descending")
            ));
            state
                .views
                .list
                .sort_by_column(Some(&name), gtk::SortType::Ascending);
            settled(&state).await;
            let info = info_named(&state, "file2.txt");
            state
                .pending_selection
                .replace(vec![model::file(&info).uri().to_string()]);
            state.select_pending();
            state.copy(true);
            let files = clipboard::read(&state.window.clipboard())
                .await
                .expect("clipboard");
            assert!(files.cut);
            assert_eq!(files.uris, vec![model::file(&info).uri().to_string()]);
            state.rename(info, "renamed.txt".into());
            until(|| {
                state
                    .views
                    .selected()
                    .first()
                    .is_some_and(|i| i.display_name() == "renamed.txt")
            })
            .await;
            until(|| state.monitor.borrow().is_some()).await;
            glib::timeout_future(Duration::from_millis(250)).await;
            let path = root.join("watched.txt");
            gio::spawn_blocking(move || std::fs::write(path, "watch"))
                .await
                .expect("worker")
                .expect("watch fixture");
            until(|| {
                state.views.selection.n_items() == 20003
                    && !state.loading.get()
                    && state
                        .views
                        .selected()
                        .first()
                        .is_some_and(|i| i.display_name() == "renamed.txt")
            })
            .await;
            eprintln!("Rename, clipboard, and live monitor passed");
            gio::prelude::ActionGroupExt::activate_action(&state.window, "hidden", None);
            until(|| state.views.selection.n_items() == 20004).await;
            state.recursive.set(true);
            state.search.set_text("nested-needle");
            until(|| {
                !state.loading.get()
                    && state.store.n_items() == 1
                    && state.views.selection.n_items() == 1
            })
            .await;
            assert_eq!(
                info_named(&state, "nested-needle.txt").display_name(),
                "nested-needle.txt"
            );
            state.clear_search();
            settled(&state).await;
            let source = gio::File::for_path(root.join("source"));
            let dest = gio::File::for_path(root.join("destination"));
            let op = state.operations.start("Smoke copy");
            assert!(ops::transfer(
                &state.window,
                vec![source.clone()],
                dest.clone(),
                false,
                &op
            )
            .await
            .expect("recursive copy"));
            op.finish();
            let copied = root.join("destination/source/nested/nested-needle.txt");
            assert_eq!(
                std::fs::read_to_string(copied).expect("copied text"),
                "preview contents"
            );
            let moved = gio::File::for_path(root.join("renamed.txt"));
            let op = state.operations.start("Smoke move");
            assert!(
                ops::transfer(&state.window, vec![moved.clone()], dest.clone(), true, &op)
                    .await
                    .expect("move")
            );
            op.finish();
            assert!(!moved.query_exists(gio::Cancellable::NONE));
            // Resolve a real conflict through the same dialog used by paste.
            glib::timeout_add_local_once(
                Duration::from_millis(100),
                glib::clone!(
                    #[weak]
                    state,
                    move || {
                        let dialog = state
                            .window
                            .visible_dialog()
                            .and_downcast::<adw::AlertDialog>()
                            .expect("conflict dialog");
                        dialog.emit_by_name::<()>("response", &[&"both"]);
                        dialog.force_close();
                    }
                ),
            );
            let op = state.operations.start("Smoke keep both");
            assert!(ops::transfer(&state.window, vec![source], dest, false, &op)
                .await
                .expect("conflict copy"));
            op.finish();
            assert!(root
                .join("destination/source (copy)/nested/nested-needle.txt")
                .exists());
            let cancel = gio::Cancellable::new();
            cancel.cancel();
            assert!(ops::query(&gio::File::for_path(&root), &cancel)
                .await
                .is_err());
            let image_path=root.join("destination/wallpaper.svg");
            gio::spawn_blocking(move||std::fs::write(image_path,r##"<svg xmlns="http://www.w3.org/2000/svg" width="800" height="600"><defs><linearGradient id="d"><stop stop-color="#7f8be6"/><stop offset="1" stop-color="#f2a46a"/></linearGradient></defs><rect width="800" height="600" fill="url(#d)"/></svg>"##)).await.expect("image worker").expect("SVG fixture");
            state.navigate(gio::File::for_path(root.join("destination")),true);settled(&state).await;
            state.set_view(ViewMode::Grid);glib::timeout_future(Duration::from_millis(400)).await;
            let svg=info_named(&state,"wallpaper.svg");let thumbnail=state.thumbs.get(&svg).await.expect("thumbnail generation").expect("image texture");
            assert_eq!((thumbnail.width(),thumbnail.height()),(256,192));
            assert!(crate::thumbs::cache_path(&glib::user_cache_dir(),model::file(&svg).uri().as_str(),true).exists());
            let texture=gtk::WidgetPaintable::new(Some(&state.toast));let snapshot=gtk::Snapshot::new();
            texture.snapshot(&snapshot,state.toast.width() as f64,state.toast.height() as f64);
            let node=snapshot.to_node().expect("rendered window contents");let surface=state.window.surface().expect("window surface");
            let renderer=gtk::gsk::Renderer::for_surface(&surface).expect("renderer");let image=renderer.render_texture(&node,None);
            let path=std::env::var_os("GAVNO_FILES_SCREENSHOT").map(PathBuf::from).unwrap_or_else(||root.join("smoke.png"));image.save_to_png(path).expect("screenshot");renderer.unrealize();
            state.window.close();
            glib::timeout_future(Duration::from_millis(100)).await;
        });
        std::fs::remove_dir_all(root).expect("remove fixture");
    }
}
