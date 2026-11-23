use crate::{
    clipboard,
    config::{Config, SortField, ViewMode},
    dock::Dock,
    icons::IconCache,
    model::{self, FileEntry, Location, ScanError},
    open,
    ops::{self, Operations},
    preview, root, root_proto, search,
    sidebar::{Sidebar, SidebarCallbacks},
    thumbs::Thumbnails,
    trash,
    views::{Report, ViewCallbacks, Views},
};
use adw::prelude::*;
use gtk::gdk;
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

thread_local! {
    static WINDOWS: RefCell<Vec<Rc<FileWindow>>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug)]
enum LoadErrorKind {
    PermissionDenied,
    Other(String),
}

struct TabState {
    id: u64,
    location: Location,
    back: Vec<Location>,
    forward: Vec<Location>,
    all_entries: Vec<FileEntry>,
    dir_writable: bool,
    filter_query: String,
    recursive: bool,
    loading: bool,
    loaded: bool,
    needs_refresh: bool,
    scroll_pos: f64,
    pending_selection: Vec<String>,
    load_cancel: Arc<AtomicBool>,
    search_cancel: Arc<AtomicBool>,
    search_revision: u64,
    monitor: Option<gio::FileMonitor>,
    debounce_source: Option<glib::SourceId>,
    load_error: Option<LoadErrorKind>,
}

impl TabState {
    fn new(id: u64, location: Location, pending_selection: Vec<String>) -> Self {
        Self {
            id,
            location,
            back: Vec::new(),
            forward: Vec::new(),
            all_entries: Vec::new(),
            dir_writable: true,
            filter_query: String::new(),
            recursive: false,
            loading: false,
            loaded: false,
            needs_refresh: false,
            scroll_pos: 0.0,
            pending_selection,
            load_cancel: Arc::new(AtomicBool::new(false)),
            search_cancel: Arc::new(AtomicBool::new(false)),
            search_revision: 0,
            monitor: None,
            debounce_source: None,
            load_error: None,
        }
    }

    fn cancel_async(&mut self) {
        self.load_cancel.store(true, Ordering::Relaxed);
        self.search_cancel.store(true, Ordering::Relaxed);
        self.monitor.take();
        if let Some(id) = self.debounce_source.take() {
            id.remove();
        }
    }
}

pub struct FileWindow {
    pub window: adw::ApplicationWindow,
    app: adw::Application,
    config: Rc<RefCell<Config>>,
    config_revision: Cell<u64>,
    config_saving: Cell<bool>,
    toast: adw::ToastOverlay,
    views: Views,
    sidebar: Sidebar,
    paned: gtk::Paned,
    tab_strip: gtk::Box,
    tabs: RefCell<Vec<TabState>>,
    active_tab: Cell<usize>,
    next_tab_id: Cell<u64>,
    visible_entries: RefCell<Vec<FileEntry>>,
    back_button: gtk::Button,
    forward_button: gtk::Button,
    up_button: gtk::Button,
    path_stack: gtk::Stack,
    crumbs_scroll: gtk::ScrolledWindow,
    crumbs: gtk::Box,
    path: gtk::Entry,
    search: gtk::Entry,
    search_button: gtk::ToggleButton,
    grid_button: gtk::Button,
    list_button: gtk::Button,
    changing: Cell<bool>,
    status: gtk::Label,
    empty: gtk::Label,
    spinner: gtk::Spinner,
    completion_revision: Cell<u64>,
    operations: Operations,
    icon_cache: IconCache,
    dock: Rc<Dock>,
    root: RefCell<Option<Rc<root::RootSession>>>,
    root_button: gtk::Button,
    root_bar: gtk::Revealer,
    root_idle: RefCell<Option<glib::SourceId>>,
    access_bar: gtk::Revealer,
    access_label: gtk::Label,
    access_button: gtk::Button,
    access_row: gtk::Box,
    who: gtk::Label,
    active_popover: RefCell<Option<gtk::Popover>>,
}

pub fn active_window(app: &adw::Application) -> Option<Rc<FileWindow>> {
    WINDOWS.with(|windows| windows.borrow().iter().find(|w| w.app == *app).cloned())
}

pub fn open(app: &adw::Application, location: Location, selected: Vec<String>) -> Rc<FileWindow> {
    let state = FileWindow::new(app, location, selected);
    WINDOWS.with(|windows| windows.borrow_mut().push(state.clone()));
    state.window.present();
    state.views.grab_focus();
    state
}

impl FileWindow {
    fn new(app: &adw::Application, initial_loc: Location, selected: Vec<String>) -> Rc<Self> {
        let config = Config::load().unwrap_or_default();
        let initial_view = config.view;
        let initial_sort = (config.sort_field, config.sort_descending);
        let initial_hidden = config.show_hidden;
        let initial_w = config.window_width.unwrap_or(1140).max(640);
        let initial_h = config.window_height.unwrap_or(720).max(460);
        let initial_side = config.sidebar_width.unwrap_or(196).clamp(150, 360);

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Files")
            .default_width(initial_w)
            .default_height(initial_h)
            .build();

        // Header bar built left-to-right matching design/files-mockup.html.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        header.add_css_class("files-header");
        let back_button = icon_button("go-previous-symbolic", "Back (Alt+Left)");
        let forward_button = icon_button("go-next-symbolic", "Forward (Alt+Right)");
        let up_button = icon_button("go-up-symbolic", "Up (Alt+Up)");
        header.append(&back_button);
        header.append(&forward_button);
        header.append(&up_button);

        let path_stack = gtk::Stack::new();
        path_stack.add_css_class("path-bar");
        path_stack.set_hexpand(true);
        let crumbs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let crumbs_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .child(&crumbs)
            .build();
        path_stack.add_named(&crumbs_scroll, Some("crumbs"));
        let path = gtk::Entry::new();
        path.set_has_frame(false);
        path.set_placeholder_text(Some("Path or file URI · Tab to complete"));
        path_stack.add_named(&path, Some("entry"));
        header.append(&path_stack);

        let search = gtk::Entry::new();
        search.set_placeholder_text(Some("Filter this folder"));
        search.set_visible(false);
        search.set_width_chars(22);
        header.append(&search);

        let search_button = gtk::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Search subfolders (Ctrl+F)")
            .build();
        header.append(&search_button);

        let grid_button = icon_button("view-grid-symbolic", "Grid view (Ctrl+1)");
        let list_button = icon_button("view-list-symbolic", "List view (Ctrl+2)");
        header.append(&grid_button);
        header.append(&list_button);

        let terminal_button = icon_button("utilities-terminal-symbolic", "Docked terminal (F4)");
        terminal_button.set_action_name(Some("win.dock"));
        header.append(&terminal_button);

        let root_button = gtk::Button::new();
        let root_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        root_content.append(&gtk::Image::from_icon_name("security-high-symbolic"));
        root_content.append(&gtk::Label::new(Some("root")));
        root_button.set_child(Some(&root_content));
        root_button.add_css_class("root-button");
        root_button.set_tooltip_text(Some("Root mode (Ctrl+Shift+R)"));
        root_button.set_action_name(Some("win.root"));
        header.append(&root_button);

        let operations = Operations::new();
        header.append(&operations.button);

        let menu = gio::Menu::new();
        menu.append(Some("New Tab"), Some("win.new-tab"));
        menu.append(Some("New Window"), Some("win.new-window"));
        menu.append(Some("New Folder"), Some("win.new-folder"));
        let view_sec = gio::Menu::new();
        view_sec.append(Some("Show Hidden Files"), Some("win.hidden"));
        let sort_menu = gio::Menu::new();
        sort_menu.append(Some("Name"), Some("win.sort-name"));
        sort_menu.append(Some("Size"), Some("win.sort-size"));
        sort_menu.append(Some("Modified"), Some("win.sort-modified"));
        sort_menu.append(Some("Type"), Some("win.sort-type"));
        sort_menu.append(Some("Descending"), Some("win.sort-desc"));
        view_sec.append_submenu(Some("Sort By"), &sort_menu);
        menu.append_section(None, &view_sec);
        let trash_sec = gio::Menu::new();
        trash_sec.append(Some("Empty Trash…"), Some("win.empty-trash"));
        trash_sec.append(Some("About Files"), Some("win.about"));
        menu.append_section(None, &trash_sec);

        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .build();
        header.append(&menu_button);
        header.append(&gtk::WindowControls::new(gtk::PackType::End));

        let handle = gtk::WindowHandle::new();
        handle.set_child(Some(&header));

        let slot: Rc<RefCell<std::rc::Weak<Self>>> = Rc::new(RefCell::new(std::rc::Weak::new()));
        let reporter: Report = Rc::new(glib::clone!(
            #[strong]
            slot,
            move |message: String| {
                if let Some(w) = slot.borrow().upgrade() {
                    w.notify(&message);
                }
            }
        ));

        let icon_cache = IconCache::new();
        let views = Views::new(
            Thumbnails::new(),
            icon_cache.clone(),
            ViewCallbacks {
                on_context_menu: Rc::new(glib::clone!(
                    #[strong]
                    slot,
                    move |pos, x, y| {
                        if let Some(w) = slot.borrow().upgrade() {
                            w.handle_context_menu(pos, x, y);
                        }
                    }
                )),
                on_middle_click: Rc::new(glib::clone!(
                    #[strong]
                    slot,
                    move |pos| {
                        if let Some(w) = slot.borrow().upgrade() {
                            w.middle_click_item(pos);
                        }
                    }
                )),
                on_sort_changed: Rc::new(glib::clone!(
                    #[strong]
                    slot,
                    move |field, desc| {
                        if let Some(w) = slot.borrow().upgrade() {
                            w.set_sort(field, desc);
                        }
                    }
                )),
                on_drop: Rc::new(glib::clone!(
                    #[strong]
                    slot,
                    move |target_dir, uris, moving, drop| {
                        if let Some(w) = slot.borrow().upgrade() {
                            w.handle_drop(
                                target_dir.map(Location::Local),
                                uris,
                                moving,
                                drop,
                            );
                        }
                    }
                )),
                on_drag_uris: Rc::new(glib::clone!(
                    #[strong]
                    slot,
                    move |pos| {
                        slot.borrow()
                            .upgrade()
                            .map(|w| w.drag_uris_for(pos))
                            .unwrap_or_default()
                    }
                )),
                on_rename: Rc::new(glib::clone!(
                    #[strong]
                    slot,
                    move |entry, new_name| {
                        if let Some(w) = slot.borrow().upgrade() {
                            w.rename_entry(entry, new_name);
                        }
                    }
                )),
                report: reporter.clone(),
            },
        );

        let tab_strip = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        tab_strip.add_css_class("tab-strip");
        tab_strip.set_visible(false);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&views.stack));
        let empty = gtk::Label::new(Some("Loading…"));
        empty.set_halign(gtk::Align::Center);
        empty.set_valign(gtk::Align::Center);
        empty.add_css_class("dim-label");
        empty.set_can_target(false);
        overlay.add_overlay(&empty);

        let dock = Dock::new(
            Rc::new(glib::clone!(
                #[strong]
                slot,
                move |path: PathBuf| {
                    if let Some(w) = slot.borrow().upgrade() {
                        let loc = Location::Local(path);
                        if loc != w.current_location() {
                            w.navigate(loc, true);
                        }
                    }
                }
            )),
            Rc::new(glib::clone!(
                #[strong]
                slot,
                move |path: PathBuf| {
                    if let Some(w) = slot.borrow().upgrade() {
                        w.external_terminal(path);
                    }
                }
            )),
            reporter.clone(),
        );

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let access_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        access_row.add_css_class("access-banner");
        access_row.append(&gtk::Image::from_icon_name("changes-prevent-symbolic"));
        let access_label = gtk::Label::new(None);
        access_label.set_hexpand(true);
        access_label.set_xalign(0.);
        access_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        access_row.append(&access_label);
        let access_button = gtk::Button::with_label("Open as root");
        access_button.add_css_class("root");
        access_button.set_action_name(Some("win.root"));
        access_row.append(&access_button);
        let access_bar = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .child(&access_row)
            .build();
        content.append(&tab_strip);
        content.append(&access_bar);
        content.append(&overlay);
        content.append(&dock.revealer);

        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        footer.add_css_class("status-footer");
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        footer.append(&spinner);
        let status = gtk::Label::new(Some("Loading…"));
        status.set_hexpand(true);
        status.set_xalign(0.);
        footer.append(&status);
        let hints = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        for (index, (key, what)) in [("F4", "terminal"), ("Shift+F4", "in window")]
            .into_iter()
            .enumerate()
        {
            if index > 0 {
                hints.append(&gtk::Label::new(Some("·")));
            }
            let keycap = gtk::Label::new(Some(key));
            keycap.add_css_class("keycap");
            hints.append(&keycap);
            hints.append(&gtk::Label::new(Some(what)));
        }
        let who = gtk::Label::new(Some(&glib::user_name().to_string_lossy()));
        who.add_css_class("who");
        who.set_margin_start(6);
        hints.append(&who);
        footer.append(&hints);
        content.append(&footer);

        let sidebar = Sidebar::build(
            &window,
            SidebarCallbacks {
                navigate: Rc::new(glib::clone!(
                    #[strong]
                    slot,
                    move |loc| {
                        if let Some(w) = slot.borrow().upgrade() {
                            w.navigate(loc, true);
                            w.views.grab_focus();
                        }
                    }
                )),
                new_tab: Rc::new(glib::clone!(
                    #[strong]
                    slot,
                    move |loc| {
                        if let Some(w) = slot.borrow().upgrade() {
                            w.new_tab(loc, true);
                        }
                    }
                )),
                open_term: Rc::new(glib::clone!(
                    #[strong]
                    slot,
                    move |path, external| {
                        if let Some(w) = slot.borrow().upgrade() {
                            if external {
                                w.external_terminal(path);
                            } else {
                                w.dock.open(&path);
                            }
                        }
                    }
                )),
                on_drop: Rc::new(glib::clone!(
                    #[strong]
                    slot,
                    move |loc, uris, moving, drop| {
                        if let Some(w) = slot.borrow().upgrade() {
                            w.handle_drop(Some(loc), uris, moving, drop);
                        }
                    }
                )),
                report: reporter,
            },
        );

        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_position(initial_side);
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);
        paned.set_start_child(Some(&sidebar.scroll));
        paned.set_end_child(Some(&content));
        paned.set_vexpand(true);

        let root_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        root_row.add_css_class("root-bar");
        let root_text = gtk::Label::new(Some(
            "Root mode: changes here affect the whole system",
        ));
        root_text.set_hexpand(true);
        root_text.set_xalign(0.);
        root_text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        root_row.append(&root_text);
        let root_exit = gtk::Button::with_label("Exit");
        root_exit.set_action_name(Some("win.root"));
        root_row.append(&root_exit);
        let root_bar = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .child(&root_row)
            .build();

        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.append(&handle);
        layout.append(&root_bar);
        layout.append(&paned);

        let toast = adw::ToastOverlay::new();
        toast.set_child(Some(&layout));
        window.set_content(Some(&toast));

        let initial_tab = TabState::new(1, initial_loc.clone(), selected);
        let this = Rc::new(Self {
            window,
            app: app.clone(),
            config: Rc::new(RefCell::new(config)),
            config_revision: Cell::new(0),
            config_saving: Cell::new(false),
            toast,
            views,
            sidebar,
            paned,
            tab_strip,
            tabs: RefCell::new(vec![initial_tab]),
            active_tab: Cell::new(0),
            next_tab_id: Cell::new(2),
            visible_entries: RefCell::new(Vec::new()),
            back_button,
            forward_button,
            up_button,
            path_stack,
            crumbs_scroll,
            crumbs,
            path,
            search,
            search_button,
            grid_button,
            list_button,
            changing: Cell::new(false),
            status,
            empty,
            spinner,
            completion_revision: Cell::new(0),
            operations,
            icon_cache,
            dock,
            root: RefCell::new(None),
            root_button,
            root_bar,
            root_idle: RefCell::new(None),
            access_bar,
            access_label,
            access_button,
            access_row,
            who,
            active_popover: RefCell::new(None),
        });
        slot.replace(Rc::downgrade(&this));

        this.actions();
        this.connect_signals();
        this.apply_view_mode(initial_view);
        this.views
            .set_sort_indicator(initial_sort.0, initial_sort.1);
        if let Some(action) = this
            .window
            .lookup_action("hidden")
            .and_downcast::<gio::SimpleAction>()
        {
            action.set_state(&initial_hidden.to_variant());
        }
        this.render_path();
        this.load_active_tab();
        this
    }

    pub fn notify(&self, message: &str) {
        self.toast.add_toast(adw::Toast::new(message));
    }

    fn close_active_popover(&self) -> bool {
        let pop = self.active_popover.borrow_mut().take();
        if let Some(pop) = pop {
            pop.popdown();
            true
        } else {
            false
        }
    }

    fn reporter(self: &Rc<Self>) -> Report {
        Rc::new(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |msg: String| this.notify(&msg)
        ))
    }

    fn current_location(&self) -> Location {
        let idx = self.active_tab.get();
        self.tabs
            .borrow()
            .get(idx)
            .map(|t| t.location.clone())
            .unwrap_or_else(|| Location::Local(glib::home_dir()))
    }

    fn active_tab_id(&self) -> u64 {
        let idx = self.active_tab.get();
        self.tabs.borrow().get(idx).map(|t| t.id).unwrap_or(0)
    }

    fn is_trash(&self) -> bool {
        self.current_location().is_trash()
    }

    fn apply_view_mode(&self, mode: ViewMode) {
        self.views.set_mode(mode);
        if mode == ViewMode::Grid {
            self.grid_button.add_css_class("on");
            self.list_button.remove_css_class("on");
        } else {
            self.list_button.add_css_class("on");
            self.grid_button.remove_css_class("on");
        }
    }

    fn set_view(self: &Rc<Self>, view: ViewMode) {
        self.apply_view_mode(view);
        self.config.borrow_mut().view = view;
        self.save_config();
        self.views.grab_focus();
    }

    fn set_sort(self: &Rc<Self>, field: SortField, descending: bool) {
        {
            let mut cfg = self.config.borrow_mut();
            if cfg.sort_field == field && cfg.sort_descending == descending {
                drop(cfg);
                self.views.set_sort_indicator(field, descending);
                return;
            }
            cfg.sort_field = field;
            cfg.sort_descending = descending;
        }
        self.views.set_sort_indicator(field, descending);
        if let Some(action) = self
            .window
            .lookup_action("sort-desc")
            .and_downcast::<gio::SimpleAction>()
        {
            action.set_state(&descending.to_variant());
        }
        self.save_config();
        let idx = self.active_tab.get();
        if let Some(tab) = self.tabs.borrow_mut().get_mut(idx) {
            model::sort_entries(&mut tab.all_entries, field, descending);
        }
        self.apply_filter_to_view(false);
    }

    fn save_config(self: &Rc<Self>) {
        self.config_revision.set(self.config_revision.get() + 1);
        if self.config_saving.replace(true) {
            return;
        }
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
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

    fn actions(self: &Rc<Self>) {
        let add = |name: &str, run: fn(&Rc<Self>)| {
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_, _| run(&this)
            ));
            self.window.add_action(&action);
        };

        add("back", |w| w.history(false));
        add("forward", |w| w.history(true));
        add("up", |w| w.up());
        add("new-tab", |w| {
            let loc = w.current_location();
            w.new_tab(loc, true);
        });
        add("close-tab", |w| {
            let idx = w.active_tab.get();
            w.close_tab(idx);
        });
        add("new-window", |w| {
            open(&w.app, w.current_location(), Vec::new());
        });
        add("new-folder", |w| w.new_folder());
        add("new-file", |w| w.new_file());
        add("copy", |w| w.copy(false));
        add("cut", |w| w.copy(true));
        add("duplicate", |w| w.duplicate());
        add("paste", |w| w.paste());
        add("dock", |w| w.toggle_dock());
        add("dock-here", |w| w.open_dock_here());
        add("terminal", |w| {
            if w.dock.has_focus() {
                if let Some(dir) = w.dock.cwd() {
                    w.external_terminal(dir);
                }
                w.dock.hide();
            } else {
                w.terminal();
            }
        });
        add("root", |w| w.toggle_root());
        add("open", |w| w.open_selected());
        add("open-tab", |w| {
            if let Some(entry) = w.views.selected().into_iter().next() {
                if entry.is_dir && entry.trash.is_none() {
                    w.new_tab(Location::Local(entry.path), true);
                }
            }
        });
        add("open-with", |w| w.open_with());
        add("quicklook", |w| w.quicklook());
        add("extract", |w| w.extract_selected());
        add("compress", |w| w.compress_selected());
        add("copy-path", |w| w.copy_selected_paths());
        add("bookmark", |w| w.toggle_bookmark_for_target());
        add("rename", |w| w.start_rename());
        add("trash", |w| w.delete(false));
        add("delete-permanently", |w| w.delete(true));
        add("properties", |w| w.properties());
        add("restore", |w| w.restore_selected());
        add("empty-trash", |w| w.empty_trash());
        add("select-all", |w| {
            w.views.selection.select_all();
        });
        add("refresh", |w| w.reload(false));
        add("sort-name", |w| {
            let desc = w.config.borrow().sort_descending;
            w.set_sort(SortField::Name, desc);
        });
        add("sort-size", |w| {
            let desc = w.config.borrow().sort_descending;
            w.set_sort(SortField::Size, desc);
        });
        add("sort-modified", |w| {
            let desc = w.config.borrow().sort_descending;
            w.set_sort(SortField::Modified, desc);
        });
        add("sort-type", |w| {
            let desc = w.config.borrow().sort_descending;
            w.set_sort(SortField::Type, desc);
        });
        add("about", |w| {
            let dialog = adw::AboutDialog::builder()
                .application_name("Files")
                .application_icon("org.gavno.Files")
                .developer_name("GAVNO")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("A file manager for the GAVNO desktop")
                .license_type(gtk::License::Gpl30)
                .build();
            dialog.present(Some(&w.window));
        });

        let hidden = gio::SimpleAction::new_stateful("hidden", None, &false.to_variant());
        hidden.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |action, _| {
                let next = !this.config.borrow().show_hidden;
                this.config.borrow_mut().show_hidden = next;
                action.set_state(&next.to_variant());
                this.save_config();
                let is_rec = {
                    let idx = this.active_tab.get();
                    this.tabs.borrow().get(idx).is_some_and(|t| t.recursive)
                };
                if is_rec {
                    this.update_search();
                } else {
                    this.apply_filter_to_view(false);
                }
            }
        ));
        self.window.add_action(&hidden);

        let sort_desc = gio::SimpleAction::new_stateful(
            "sort-desc",
            None,
            &self.config.borrow().sort_descending.to_variant(),
        );
        sort_desc.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, _| {
                let (field, desc) = {
                    let c = this.config.borrow();
                    (c.sort_field, !c.sort_descending)
                };
                this.set_sort(field, desc);
            }
        ));
        self.window.add_action(&sort_desc);
    }

    fn connect_signals(self: &Rc<Self>) {
        self.window.connect_close_request(glib::clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                let w = this.window.width();
                let h = this.window.height();
                let sw = this.paned.position();
                {
                    let mut cfg = this.config.borrow_mut();
                    if w >= 640 && h >= 460 {
                        cfg.window_width = Some(w);
                        cfg.window_height = Some(h);
                    }
                    if sw >= 140 {
                        cfg.sidebar_width = Some(sw);
                    }
                }
                let _ = this.config.borrow().save();
                glib::Propagation::Proceed
            }
        ));

        self.window.connect_destroy(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| {
                this.close_active_popover();
                this.views.dismiss_rename();
                this.views.clear();
                for tab in this.tabs.borrow_mut().iter_mut() {
                    tab.cancel_async();
                }
                this.operations.cancel_all();
                if let Some(id) = this.root_idle.borrow_mut().take() {
                    id.remove();
                }
                this.root.borrow_mut().take();
                WINDOWS.with(|windows| windows.borrow_mut().retain(|w| !Rc::ptr_eq(w, &this)));
            }
        ));

        self.back_button.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| {
                this.history(false);
                this.views.grab_focus();
            }
        ));
        self.forward_button.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| {
                this.history(true);
                this.views.grab_focus();
            }
        ));
        self.up_button.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| {
                this.up();
                this.views.grab_focus();
            }
        ));
        self.grid_button.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.set_view(ViewMode::Grid)
        ));
        self.list_button.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.set_view(ViewMode::List)
        ));

        // Clicking empty space in the breadcrumb bar switches to editable path entry.
        let path_click = gtk::GestureClick::new();
        path_click.set_button(1);
        path_click.connect_pressed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, _, x, y| {
                if this.path_stack.visible_child_name().as_deref() == Some("crumbs") {
                    if let Some(picked) = this.crumbs_scroll.pick(x, y, gtk::PickFlags::DEFAULT) {
                        if picked.ancestor(gtk::Button::static_type()).is_some() {
                            return;
                        }
                    }
                    this.enter_path_mode();
                }
            }
        ));
        self.crumbs_scroll.add_controller(path_click);

        self.path.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |entry| {
                let value = entry.text();
                if value.contains('\0') {
                    this.notify("Paths cannot contain a null character");
                    return;
                }
                match this.resolve_path(&value) {
                    Ok(loc) => {
                        this.path_stack.set_visible_child_name("crumbs");
                        this.navigate(loc, true);
                        this.views.grab_focus();
                    }
                    Err(e) => this.notify(&e),
                }
            }
        ));

        let path_keys = gtk::EventControllerKey::new();
        path_keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    if this.close_active_popover() {
                        return glib::Propagation::Stop;
                    }
                    this.path_stack.set_visible_child_name("crumbs");
                    this.views.grab_focus();
                    glib::Propagation::Stop
                } else if key == gdk::Key::Tab {
                    this.complete_path();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        self.path.add_controller(path_keys);

        self.search_button.connect_toggled(glib::clone!(
            #[weak(rename_to = this)]
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
            #[weak(rename_to = this)]
            self,
            move |_| {
                if !this.changing.get() {
                    this.update_search();
                }
            }
        ));

        self.search.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| {
                if let Some(entry) = this.views.entry_at(0) {
                    this.open_item(entry);
                }
            }
        ));

        let search_keys = gtk::EventControllerKey::new();
        search_keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    this.search_button.set_active(false);
                    this.views.grab_focus();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        self.search.add_controller(search_keys);

        self.views.grid.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, position| this.activate(position)
        ));
        self.views.list.connect_activate(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, position| this.activate(position)
        ));
        self.views.selection.connect_selection_changed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, _, _| this.update_status()
        ));

        // Mouse side buttons (8 = Back, 9 = Forward).
        let mouse_nav = gtk::GestureClick::new();
        mouse_nav.set_button(0);
        mouse_nav.set_propagation_phase(gtk::PropagationPhase::Capture);
        mouse_nav.connect_pressed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |gesture, _, _, _| match gesture.current_button() {
                8 => {
                    this.history(false);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
                9 => {
                    this.history(true);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
                _ => {}
            }
        ));
        self.window.add_controller(mouse_nav);

        // Window-wide shortcuts in Capture phase with strict guards for terminal, entries, popovers & dialogs.
        let win_keys = gtk::EventControllerKey::new();
        win_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        win_keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, state| this.handle_window_key(key, state)
        ));
        self.window.add_controller(win_keys);
    }

    fn handle_window_key(self: &Rc<Self>, key: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        // Never interfere with modal AdwDialogs (Confirm, New Folder, Conflict, Polkit password prompt).
        if self.window.visible_dialog().is_some() {
            return glib::Propagation::Proceed;
        }
        let focus = GtkWindowExt::focus(&self.window);
        let in_popover = focus
            .as_ref()
            .is_some_and(|w| w.ancestor(gtk::Popover::static_type()).is_some());
        if in_popover {
            return glib::Propagation::Proceed;
        }

        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let alt = state.contains(gdk::ModifierType::ALT_MASK);
        let super_mod = state.contains(gdk::ModifierType::SUPER_MASK);

        // Terminal toggle / pop-out always works even while typing in terminal or entries.
        if key == gdk::Key::F4 && !ctrl && !alt && !super_mod {
            if shift {
                if self.dock.has_focus() {
                    if let Some(dir) = self.dock.cwd() {
                        self.external_terminal(dir);
                    }
                    self.dock.hide();
                } else {
                    self.terminal();
                }
            } else {
                self.toggle_dock();
            }
            return glib::Propagation::Stop;
        }

        #[cfg(feature = "vte")]
        let in_terminal = focus.as_ref().is_some_and(|w| {
            w.is::<vte4::Terminal>() || w.ancestor(vte4::Terminal::static_type()).is_some()
        });
        #[cfg(not(feature = "vte"))]
        let in_terminal = false;
        if in_terminal {
            return glib::Propagation::Proceed;
        }

        let in_text = focus.as_ref().is_some_and(|w| {
            w.is::<gtk::Entry>()
                || w.is::<gtk::SearchEntry>()
                || w.is::<gtk::PasswordEntry>()
                || w.is::<gtk::Text>()
                || w.is::<gtk::TextView>()
                || w.ancestor(gtk::Entry::static_type()).is_some()
                || w.ancestor(gtk::Text::static_type()).is_some()
                || w.ancestor(gtk::TextView::static_type()).is_some()
        });
        if in_text {
            return glib::Propagation::Proceed;
        }

        if super_mod {
            return glib::Propagation::Proceed;
        }

        let lower = key.to_lower();

        if ctrl && !alt {
            match (shift, lower) {
                (false, gdk::Key::l) => {
                    self.enter_path_mode();
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::f) => {
                    self.enter_recursive_search();
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::t) => {
                    let loc = self.current_location();
                    self.new_tab(loc, true);
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::w) => {
                    let idx = self.active_tab.get();
                    self.close_tab(idx);
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::Tab) => {
                    self.cycle_tab(1);
                    return glib::Propagation::Stop;
                }
                (true, gdk::Key::ISO_Left_Tab) | (true, gdk::Key::Tab) => {
                    self.cycle_tab(-1);
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::n) => {
                    open(&self.app, self.current_location(), Vec::new());
                    return glib::Propagation::Stop;
                }
                (true, gdk::Key::n) => {
                    self.new_folder();
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::_1) => {
                    self.set_view(ViewMode::Grid);
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::_2) => {
                    self.set_view(ViewMode::List);
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::h) => {
                    let _ = gio::prelude::ActionGroupExt::activate_action(
                        &self.window,
                        "hidden",
                        None,
                    );
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::c) => {
                    self.copy(false);
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::x) => {
                    self.copy(true);
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::v) => {
                    self.paste();
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::d) => {
                    self.duplicate();
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::a) => {
                    self.views.selection.select_all();
                    return glib::Propagation::Stop;
                }
                (false, gdk::Key::r) => {
                    self.reload(false);
                    return glib::Propagation::Stop;
                }
                (true, gdk::Key::r) => {
                    self.toggle_root();
                    return glib::Propagation::Stop;
                }
                _ => return glib::Propagation::Proceed,
            }
        }

        if alt && !ctrl && !shift {
            match key {
                gdk::Key::Left => {
                    self.history(false);
                    return glib::Propagation::Stop;
                }
                gdk::Key::Right => {
                    self.history(true);
                    return glib::Propagation::Stop;
                }
                gdk::Key::Up => {
                    self.up();
                    return glib::Propagation::Stop;
                }
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    self.properties();
                    return glib::Propagation::Stop;
                }
                _ => return glib::Propagation::Proceed,
            }
        }

        if ctrl || alt {
            return glib::Propagation::Proceed;
        }

        let in_file_view = focus.as_ref().is_some_and(|w| {
            w.is::<gtk::GridView>()
                || w.is::<gtk::ColumnView>()
                || w.ancestor(gtk::GridView::static_type()).is_some()
                || w.ancestor(gtk::ColumnView::static_type()).is_some()
        });

        match key {
            gdk::Key::F2 if !shift => {
                self.start_rename();
                glib::Propagation::Stop
            }
            gdk::Key::F5 if !shift => {
                self.reload(false);
                glib::Propagation::Stop
            }
            gdk::Key::Delete => {
                self.delete(shift);
                glib::Propagation::Stop
            }
            gdk::Key::BackSpace if !shift => {
                self.history(false);
                glib::Propagation::Stop
            }
            gdk::Key::Escape if !shift => {
                if self.close_active_popover() {
                    return glib::Propagation::Stop;
                }
                if self.search_button.is_active() {
                    self.search_button.set_active(false);
                } else {
                    self.views.selection.unselect_all();
                }
                self.views.grab_focus();
                glib::Propagation::Stop
            }
            gdk::Key::space if !shift && in_file_view => {
                self.quicklook();
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter if !shift && in_file_view => {
                self.open_selected();
                glib::Propagation::Stop
            }
            _ => {
                if self.active_popover.borrow().is_none() {
                    if let Some(c) = key
                        .to_unicode()
                        .filter(|c| !c.is_control() && !c.is_whitespace())
                    {
                        let was_recursive = {
                            let mut tabs = self.tabs.borrow_mut();
                            if let Some(tab) = tabs.get_mut(self.active_tab.get()) {
                                tab.search_cancel.store(true, Ordering::Relaxed);
                                tab.search_revision += 1;
                                std::mem::replace(&mut tab.recursive, false)
                            } else {
                                false
                            }
                        };
                        self.changing.set(true);
                        self.search.set_placeholder_text(Some("Filter this folder"));
                        self.search_button.set_active(true);
                        self.search.set_visible(true);
                        self.search.grab_focus();
                        let next = if was_recursive {
                            c.to_string()
                        } else {
                            format!("{}{c}", self.search.text())
                        };
                        if was_recursive {
                            if let Some(tab) = self.tabs.borrow_mut().get_mut(self.active_tab.get())
                            {
                                tab.filter_query = next.clone();
                            }
                        }
                        self.search.set_text(&next);
                        self.changing.set(false);
                        self.search.set_position(-1);
                        if was_recursive {
                            self.load_active_tab();
                        } else {
                            self.update_search();
                        }
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            }
        }
    }

    fn enter_path_mode(&self) {
        self.close_active_popover();
        self.views.dismiss_rename();
        self.path.set_text(&self.current_location().parse_name());
        self.path_stack.set_visible_child_name("entry");
        self.path.grab_focus();
        self.path.select_region(0, -1);
    }

    fn enter_recursive_search(self: &Rc<Self>) {
        let is_trash = {
            let mut tabs = self.tabs.borrow_mut();
            if let Some(tab) = tabs.get_mut(self.active_tab.get()) {
                if let Some(id) = tab.debounce_source.take() {
                    id.remove();
                }
                let trash = tab.location.is_trash();
                tab.recursive = !trash;
                trash
            } else {
                false
            }
        };
        self.search.set_placeholder_text(Some(if is_trash {
            "Filter Trash"
        } else {
            "Search subfolders · 10,000 results max"
        }));
        self.search_button.set_active(true);
        self.search.grab_focus();
        self.update_search();
    }

    fn render_tabs(self: &Rc<Self>) {
        while let Some(child) = self.tab_strip.first_child() {
            self.tab_strip.remove(&child);
        }
        let snapshot: Vec<(usize, u64, String)> = {
            let tabs = self.tabs.borrow();
            if tabs.len() <= 1 {
                self.tab_strip.set_visible(false);
                return;
            }
            tabs.iter()
                .enumerate()
                .map(|(i, t)| (i, t.id, t.location.display_title()))
                .collect()
        };
        let active = self.active_tab.get();
        self.tab_strip.set_visible(true);

        for (idx, _id, title) in snapshot {
            let pill = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            pill.add_css_class("tab-pill");
            if idx == active {
                pill.add_css_class("active");
            }
            let btn = gtk::Button::with_label(&title);
            btn.add_css_class("tab-label");
            if let Some(lbl) = btn.child().and_downcast::<gtk::Label>() {
                lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
                lbl.set_max_width_chars(22);
            }
            btn.connect_clicked(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_| this.switch_tab(idx)
            ));
            let mid = gtk::GestureClick::new();
            mid.set_button(2);
            mid.connect_pressed(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |gesture, _, _, _| {
                    this.close_tab(idx);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            ));
            btn.add_controller(mid);

            let close = gtk::Button::from_icon_name("window-close-symbolic");
            close.add_css_class("tab-close");
            close.set_tooltip_text(Some("Close tab (Ctrl+W)"));
            close.connect_clicked(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_| this.close_tab(idx)
            ));

            pill.append(&btn);
            pill.append(&close);
            self.tab_strip.append(&pill);
        }

        let add_btn = gtk::Button::from_icon_name("list-add-symbolic");
        add_btn.add_css_class("flat");
        add_btn.add_css_class("tab-add");
        add_btn.set_tooltip_text(Some("New tab (Ctrl+T)"));
        add_btn.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| {
                let loc = this.current_location();
                this.new_tab(loc, true);
            }
        ));
        self.tab_strip.append(&add_btn);
    }

    fn new_tab(self: &Rc<Self>, loc: Location, switch: bool) {
        let id = self.next_tab_id.get();
        self.next_tab_id.set(id + 1);
        let new_idx = {
            let mut tabs = self.tabs.borrow_mut();
            tabs.push(TabState::new(id, loc, Vec::new()));
            tabs.len() - 1
        };
        if switch {
            self.switch_tab(new_idx);
            self.load_active_tab();
        } else {
            self.render_tabs();
        }
    }

    fn switch_tab(self: &Rc<Self>, idx: usize) {
        self.close_active_popover();
        self.views.dismiss_rename();
        let cur = self.active_tab.get();
        let len = self.tabs.borrow().len();
        if idx >= len {
            return;
        }
        if cur != idx {
            let scroll = self.views.scroll_value();
            if let Some(prev) = self.tabs.borrow_mut().get_mut(cur) {
                prev.scroll_pos = scroll;
            }
        }
        self.active_tab.set(idx);
        let (sort_field, sort_desc) = {
            let c = self.config.borrow();
            (c.sort_field, c.sort_descending)
        };
        let (query, recursive, loc, scroll_pos, loading, loaded, needs_refresh) = {
            let mut tabs = self.tabs.borrow_mut();
            let tab = &mut tabs[idx];
            if tab.loaded && !tab.recursive {
                model::sort_entries(&mut tab.all_entries, sort_field, sort_desc);
            }
            (
                tab.filter_query.clone(),
                tab.recursive,
                tab.location.clone(),
                tab.scroll_pos,
                tab.loading,
                tab.loaded,
                tab.needs_refresh,
            )
        };
        self.changing.set(true);
        self.search.set_text(&query);
        let search_open = !query.is_empty() || recursive;
        self.search_button.set_active(search_open);
        self.search.set_visible(search_open);
        self.search.set_placeholder_text(Some(if recursive {
            "Search subfolders · 10,000 results max"
        } else {
            "Filter this folder"
        }));
        self.changing.set(false);

        if loading {
            self.spinner.set_visible(true);
            self.spinner.start();
        } else {
            self.spinner.stop();
            self.spinner.set_visible(false);
        }

        if let Some(path) = loc.local_path() {
            self.dock.follow(path);
        }
        self.render_tabs();
        self.render_path();
        self.apply_filter_to_view(false);
        self.views.set_scroll_value(scroll_pos);
        self.update_access_banner();
        self.views.grab_focus();

        if (!loaded || needs_refresh) && !loading && !recursive {
            self.load_tab(idx, needs_refresh && loaded);
        }
    }

    fn close_tab(self: &Rc<Self>, idx: usize) {
        let len = self.tabs.borrow().len();
        if len <= 1 {
            self.window.close();
            return;
        }
        if idx >= len {
            return;
        }
        let cur = self.active_tab.get();
        {
            let mut tabs = self.tabs.borrow_mut();
            let mut removed = tabs.remove(idx);
            removed.cancel_async();
        }
        let next_idx = if idx < cur {
            cur - 1
        } else if idx == cur {
            cur.min(len - 2)
        } else {
            cur
        };
        self.active_tab.set(next_idx);
        if idx == cur {
            self.switch_tab(next_idx);
        } else {
            self.render_tabs();
        }
    }

    fn cycle_tab(self: &Rc<Self>, delta: isize) {
        let len = self.tabs.borrow().len();
        if len <= 1 {
            return;
        }
        let cur = self.active_tab.get() as isize;
        let next = (cur + delta).rem_euclid(len as isize) as usize;
        self.switch_tab(next);
    }

    fn history(self: &Rc<Self>, forward: bool) {
        let idx = self.active_tab.get();
        let target = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.get_mut(idx) else {
                return;
            };
            let next = if forward {
                tab.forward.pop()
            } else {
                tab.back.pop()
            };
            let Some(loc) = next else {
                return;
            };
            let cur = tab.location.clone();
            if forward {
                tab.back.push(cur);
            } else {
                tab.forward.push(cur);
            }
            loc
        };
        self.navigate(target, false);
    }

    fn up(self: &Rc<Self>) {
        if let Some(parent) = self.current_location().parent() {
            self.navigate(parent, true);
        }
    }

    fn resolve_path(&self, text: &str) -> Result<Location, String> {
        let text = text.trim();
        if text == "~" {
            return Ok(Location::Local(glib::home_dir()));
        }
        if let Some(tail) = text.strip_prefix("~/") {
            return Ok(Location::Local(glib::home_dir().join(tail)));
        }
        if text == "trash:" || text.starts_with("trash://") {
            return Ok(Location::Trash);
        }
        if text.contains("://") {
            return Location::from_uri(text);
        }
        if text.starts_with('/') {
            return Ok(Location::Local(PathBuf::from(text)));
        }
        match self.current_location() {
            Location::Local(cur) => Ok(Location::Local(cur.join(text))),
            Location::Trash => Ok(Location::Local(glib::home_dir().join(text))),
        }
    }

    pub fn navigate(self: &Rc<Self>, location: Location, record_history: bool) {
        self.close_active_popover();
        self.views.dismiss_rename();
        let idx = self.active_tab.get();
        {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.get_mut(idx) else {
                return;
            };
            if record_history && tab.location != location {
                let prev = tab.location.clone();
                tab.back.push(prev);
                tab.forward.clear();
            }
            tab.location = location.clone();
            tab.search_cancel.store(true, Ordering::Relaxed);
            tab.recursive = false;
            tab.filter_query.clear();
            tab.loaded = false;
            tab.needs_refresh = false;
            tab.scroll_pos = 0.0;
            tab.load_error = None;
        }
        if let Some(path) = location.local_path() {
            self.dock.follow(path);
        }
        self.changing.set(true);
        self.search.set_text("");
        self.search_button.set_active(false);
        self.search.set_visible(false);
        self.search.set_placeholder_text(Some("Filter this folder"));
        self.changing.set(false);
        self.render_tabs();
        self.render_path();
        self.load_active_tab();
    }

    fn render_path(self: &Rc<Self>) {
        self.close_active_popover();
        let idx = self.active_tab.get();
        let (has_back, has_fwd, loc) = {
            let tabs = self.tabs.borrow();
            let Some(tab) = tabs.get(idx) else {
                return;
            };
            (!tab.back.is_empty(), !tab.forward.is_empty(), tab.location.clone())
        };
        self.back_button.set_sensitive(has_back);
        self.forward_button.set_sensitive(has_fwd);
        self.up_button.set_sensitive(loc.parent().is_some());

        while let Some(child) = self.crumbs.first_child() {
            self.crumbs.remove(&child);
        }
        self.window
            .set_title(Some(&format!("{} — Files", loc.display_title())));

        match &loc {
            Location::Local(path) => {
                for (label, seg_path) in model::breadcrumbs(path, &glib::home_dir()) {
                    let button = gtk::Button::with_label(&label);
                    button.add_css_class("flat");
                    button.add_css_class("crumb");
                    button.connect_clicked(glib::clone!(
                        #[weak(rename_to = this)]
                        self,
                        move |_| {
                            this.navigate(Location::Local(seg_path.clone()), true);
                            this.views.grab_focus();
                        }
                    ));
                    if self.crumbs.first_child().is_some() {
                        let sep = gtk::Label::new(Some("›"));
                        sep.add_css_class("crumb-sep");
                        self.crumbs.append(&sep);
                    }
                    self.crumbs.append(&button);
                }
                if let Some(last) = self.crumbs.last_child() {
                    last.add_css_class("current");
                }
            }
            Location::Trash => {
                let lbl = gtk::Label::new(Some("Trash"));
                lbl.set_margin_start(8);
                lbl.add_css_class("crumb");
                lbl.add_css_class("current");
                self.crumbs.append(&lbl);
            }
        }
        self.path_stack.set_visible_child_name("crumbs");
        self.sidebar.set_active(&loc);

        // Scroll breadcrumb strip to the right so the active folder is always visible.
        let scroll = self.crumbs_scroll.clone();
        glib::idle_add_local_once(move || {
            let adj = scroll.hadjustment();
            adj.set_value((adj.upper() - adj.page_size()).max(0.0));
        });
    }

    fn reload(self: &Rc<Self>, silent: bool) {
        let idx = self.active_tab.get();
        let is_rec = self.tabs.borrow().get(idx).is_some_and(|t| t.recursive);
        if is_rec {
            self.update_search();
            return;
        }
        if !silent {
            let has_pending = self
                .tabs
                .borrow()
                .get(idx)
                .is_some_and(|t| !t.pending_selection.is_empty());
            if !has_pending {
                let uris: Vec<String> = self
                    .views
                    .selected()
                    .into_iter()
                    .map(|e| e.uri)
                    .collect();
                let scroll = self.views.scroll_value();
                if let Some(tab) = self.tabs.borrow_mut().get_mut(idx) {
                    tab.pending_selection = uris;
                    tab.scroll_pos = scroll;
                }
            }
        }
        self.load_tab(idx, silent);
    }

    fn load_active_tab(self: &Rc<Self>) {
        let idx = self.active_tab.get();
        self.load_tab(idx, false);
    }

    fn load_tab(self: &Rc<Self>, idx: usize, silent: bool) {
        let (tab_id, loc, cancel) = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.get_mut(idx) else {
                return;
            };
            tab.load_cancel.store(true, Ordering::Relaxed);
            tab.needs_refresh = false;
            tab.loading = true;
            if !silent {
                tab.monitor.take();
                if let Some(s) = tab.debounce_source.take() {
                    s.remove();
                }
                tab.all_entries.clear();
                tab.load_error = None;
                tab.loaded = false;
            }
            let c = Arc::new(AtomicBool::new(false));
            tab.load_cancel = c.clone();
            (tab.id, tab.location.clone(), c)
        };

        let is_active = self.active_tab_id() == tab_id;
        if !silent && is_active {
            self.views.clear();
            self.visible_entries.borrow_mut().clear();
            self.spinner.set_visible(true);
            self.spinner.start();
            self.access_bar.set_reveal_child(false);
            self.update_status();
        }

        let root_session = self.root.borrow().clone();
        let (sort_field, sort_desc) = {
            let c = self.config.borrow();
            (c.sort_field, c.sort_descending)
        };

        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                enum Outcome {
                    Entries {
                        entries: Vec<FileEntry>,
                        writable: bool,
                        trash_total: Option<usize>,
                    },
                    Err(LoadErrorKind),
                    Cancelled,
                }

                let outcome = match (&root_session, &loc) {
                    (Some(root), Location::Local(path)) => {
                        this.touch_root();
                        match root.list(path).await {
                            Ok(raw) => {
                                let mut entries = root::file_entries(path, &raw);
                                model::sort_entries(&mut entries, sort_field, sort_desc);
                                Outcome::Entries {
                                    entries,
                                    writable: true,
                                    trash_total: None,
                                }
                            }
                            Err(e) => Outcome::Err(LoadErrorKind::Other(e)),
                        }
                    }
                    (_, Location::Trash) => {
                        let c = cancel.clone();
                        match gio::spawn_blocking(move || trash::list_trash(&c)).await {
                            Ok(Ok(mut entries)) => {
                                let total = entries.len();
                                model::sort_entries(&mut entries, sort_field, sort_desc);
                                Outcome::Entries {
                                    entries,
                                    writable: true,
                                    trash_total: Some(total),
                                }
                            }
                            Ok(Err(e)) => Outcome::Err(LoadErrorKind::Other(e)),
                            Err(_) => {
                                Outcome::Err(LoadErrorKind::Other("Trash worker failed".into()))
                            }
                        }
                    }
                    (None, Location::Local(path)) => {
                        let p = path.clone();
                        let c = cancel.clone();
                        match gio::spawn_blocking(move || model::scan_directory(&p, false, &c))
                            .await
                        {
                            Ok(Ok(mut res)) => {
                                model::sort_entries(&mut res.entries, sort_field, sort_desc);
                                Outcome::Entries {
                                    entries: res.entries,
                                    writable: res.dir_writable,
                                    trash_total: None,
                                }
                            }
                            Ok(Err(ScanError::Cancelled)) => Outcome::Cancelled,
                            Ok(Err(ScanError::PermissionDenied)) => {
                                Outcome::Err(LoadErrorKind::PermissionDenied)
                            }
                            Ok(Err(ScanError::NotFound(e) | ScanError::Other(e))) => {
                                Outcome::Err(LoadErrorKind::Other(e))
                            }
                            Err(_) => {
                                Outcome::Err(LoadErrorKind::Other("Directory worker failed".into()))
                            }
                        }
                    }
                };

                if cancel.load(Ordering::Relaxed) || matches!(outcome, Outcome::Cancelled) {
                    let mut stop_spinner = false;
                    if let Some(tab) = this.tabs.borrow_mut().iter_mut().find(|t| t.id == tab_id) {
                        if Arc::ptr_eq(&tab.load_cancel, &cancel) {
                            tab.loading = false;
                            stop_spinner = this.active_tab_id() == tab_id;
                        }
                    }
                    if stop_spinner {
                        this.spinner.stop();
                        this.spinner.set_visible(false);
                        this.update_status();
                    }
                    return;
                }

                // Re-locate tab by immutable tab_id.
                let Some(tab_pos) = this.tabs.borrow().iter().position(|t| t.id == tab_id) else {
                    return;
                };

                let (preserved_sel, preserved_scroll) = if silent && this.active_tab_id() == tab_id
                {
                    (
                        this.views
                            .selected()
                            .into_iter()
                            .map(|e| e.uri)
                            .collect::<Vec<_>>(),
                        Some(this.views.scroll_value()),
                    )
                } else {
                    (Vec::new(), None)
                };

                let mut monitor_needed = false;
                let mut err_to_toast = None;
                let replay_refresh;
                {
                    let mut tabs = this.tabs.borrow_mut();
                    let tab = &mut tabs[tab_pos];
                    tab.loading = false;
                    replay_refresh = std::mem::replace(&mut tab.needs_refresh, false);
                    match outcome {
                        Outcome::Entries {
                            entries,
                            writable,
                            trash_total,
                        } => {
                            if let Some(count) = trash_total {
                                this.sidebar.set_trash_count(count);
                            }
                            tab.all_entries = entries;
                            tab.dir_writable = writable;
                            tab.load_error = None;
                            tab.loaded = true;
                            if silent && !preserved_sel.is_empty() && tab.pending_selection.is_empty()
                            {
                                tab.pending_selection = preserved_sel;
                            }
                            if let Some(sc) = preserved_scroll {
                                tab.scroll_pos = sc;
                            }
                            monitor_needed = tab.monitor.is_none() && root_session.is_none();
                        }
                        Outcome::Err(kind) => {
                            tab.loaded = true;
                            tab.all_entries.clear();
                            if !silent {
                                if let LoadErrorKind::Other(ref msg) = kind {
                                    err_to_toast = Some(msg.clone());
                                }
                            }
                            tab.load_error = Some(kind);
                        }
                        Outcome::Cancelled => unreachable!(),
                    }
                }

                if let Some(msg) = err_to_toast {
                    this.notify(&msg);
                }

                if this.active_tab_id() == tab_id {
                    this.spinner.stop();
                    this.spinner.set_visible(false);
                    this.apply_filter_to_view(!silent);
                    this.update_access_banner();
                }

                if monitor_needed {
                    this.attach_monitor(tab_id, &loc);
                }

                if replay_refresh {
                    if this.active_tab_id() == tab_id {
                        this.schedule_silent_refresh(tab_id);
                    } else if let Some(tab) =
                        this.tabs.borrow_mut().iter_mut().find(|t| t.id == tab_id)
                    {
                        tab.needs_refresh = true;
                    }
                }
            }
        ));
    }

    fn attach_monitor(self: &Rc<Self>, tab_id: u64, loc: &Location) {
        let watch_path = match loc {
            Location::Local(p) => p.clone(),
            Location::Trash => trash::trash_root().join("files"),
        };
        let Ok(mon) = gio::File::for_path(&watch_path)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        else {
            return;
        };
        mon.connect_changed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_, _, _, event| {
                if matches!(
                    event,
                    gio::FileMonitorEvent::AttributeChanged
                        | gio::FileMonitorEvent::ChangesDoneHint
                ) {
                    return;
                }
                this.schedule_silent_refresh(tab_id);
            }
        ));
        if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|t| t.id == tab_id) {
            tab.monitor = Some(mon);
        }
    }

    fn schedule_silent_refresh(self: &Rc<Self>, tab_id: u64) {
        let is_active = self.active_tab_id() == tab_id;
        let mut tabs = self.tabs.borrow_mut();
        let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) else {
            return;
        };
        if tab.recursive {
            return;
        }
        if tab.loading || !is_active {
            tab.needs_refresh = true;
            return;
        }
        if let Some(id) = tab.debounce_source.take() {
            id.remove();
        }
        let source = glib::timeout_add_local_once(
            Duration::from_millis(120),
            glib::clone!(
                #[weak(rename_to = this)]
                self,
                move || {
                    let idx_opt = {
                        let mut tabs = this.tabs.borrow_mut();
                        if let Some(pos) = tabs.iter().position(|t| t.id == tab_id) {
                            tabs[pos].debounce_source = None;
                            (!tabs[pos].recursive).then_some(pos)
                        } else {
                            None
                        }
                    };
                    if let Some(pos) = idx_opt {
                        this.load_tab(pos, true);
                    }
                }
            ),
        );
        tab.debounce_source = Some(source);
    }

    fn apply_filter_to_view(&self, scroll_to_pending: bool) {
        let idx = self.active_tab.get();
        let (show_hidden, query, entries_snapshot, pending, saved_scroll) = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.get_mut(idx) else {
                return;
            };
            let show_hidden = self.config.borrow().show_hidden;
            let q = tab.filter_query.trim().to_lowercase();
            let filtered: Vec<FileEntry> = tab
                .all_entries
                .iter()
                .filter(|e| {
                    (show_hidden || !e.is_hidden)
                        && (q.is_empty() || e.lower_name.contains(&q))
                })
                .cloned()
                .collect();
            let pending = std::mem::take(&mut tab.pending_selection);
            (show_hidden, q, filtered, pending, tab.scroll_pos)
        };
        let _ = (show_hidden, query);

        self.visible_entries.replace(entries_snapshot.clone());
        self.views.replace_entries(&entries_snapshot);
        self.update_status();

        if !pending.is_empty() {
            let views = &self.views;
            views.select_by_uris(&pending, scroll_to_pending);
        }
        if !scroll_to_pending && saved_scroll > 0.0 {
            self.views.set_scroll_value(saved_scroll);
        }
    }

    fn update_access_banner(&self) {
        let idx = self.active_tab.get();
        let is_root = self.root.borrow().is_some();
        let (loc, writable, err) = {
            let tabs = self.tabs.borrow();
            let Some(tab) = tabs.get(idx) else {
                return;
            };
            (tab.location.clone(), tab.dir_writable, tab.load_error.clone())
        };
        self.access_row.remove_css_class("readonly-banner");
        match err {
            Some(LoadErrorKind::PermissionDenied) if !is_root => {
                self.access_label.set_text(&format!(
                    "No access to {}: only root can read this folder.",
                    loc.parse_name()
                ));
                self.access_button.set_label("Open as root");
                self.access_bar.set_reveal_child(true);
            }
            _ if !writable && !is_root && !loc.is_trash() => {
                self.access_row.add_css_class("readonly-banner");
                self.access_label
                    .set_text("Read-only folder. Turn on root mode to edit files here.");
                self.access_button.set_label("Root (Ctrl+Shift+R)");
                self.access_bar.set_reveal_child(true);
            }
            _ => {
                self.access_bar.set_reveal_child(false);
            }
        }
    }

    fn update_status(&self) {
        let idx = self.active_tab.get();
        let (loading, query, is_trash, load_err) = {
            let tabs = self.tabs.borrow();
            let Some(tab) = tabs.get(idx) else {
                return;
            };
            (
                tab.loading,
                tab.filter_query.clone(),
                tab.location.is_trash(),
                tab.load_error.clone(),
            )
        };
        let count = self.views.n_items();
        let selected = self.views.selected_count();
        self.status.set_text(&if selected > 0 {
            let bytes = self.views.selected_file_size_sum();
            format!("{selected} of {count} selected · {}", model::format_size(bytes))
        } else {
            format!(
                "{count} items{}",
                if loading { " · Loading…" } else { "" }
            )
        });
        self.empty.set_visible(count == 0);
        self.empty.set_text(if loading {
            "Loading…"
        } else if load_err.is_some() {
            "Unable to open this folder"
        } else if !query.is_empty() {
            "Nothing found"
        } else if is_trash {
            "Trash is empty"
        } else {
            "This folder is empty"
        });
    }

    fn clear_search(self: &Rc<Self>) {
        let idx = self.active_tab.get();
        let was_recursive = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.get_mut(idx) else {
                return;
            };
            tab.search_cancel.store(true, Ordering::Relaxed);
            tab.search_revision += 1;
            tab.filter_query.clear();
            std::mem::replace(&mut tab.recursive, false)
        };
        self.changing.set(true);
        self.search.set_text("");
        self.changing.set(false);
        if was_recursive {
            self.load_active_tab();
        } else {
            self.apply_filter_to_view(false);
        }
    }

    fn update_search(self: &Rc<Self>) {
        let query = self.search.text().to_string();
        let idx = self.active_tab.get();
        let (tab_id, recursive, cancel, revision, loc) = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.get_mut(idx) else {
                return;
            };
            tab.filter_query = query.clone();
            if !tab.recursive {
                drop(tabs);
                self.apply_filter_to_view(false);
                return;
            }
            tab.search_cancel.store(true, Ordering::Relaxed);
            tab.load_cancel.store(true, Ordering::Relaxed);
            tab.load_cancel = Arc::new(AtomicBool::new(false));
            let next_cancel = Arc::new(AtomicBool::new(false));
            tab.search_cancel = next_cancel.clone();
            tab.search_revision += 1;
            if query.trim().is_empty() {
                tab.loading = false;
                tab.all_entries.clear();
            }
            (
                tab.id,
                tab.recursive,
                next_cancel,
                tab.search_revision,
                tab.location.clone(),
            )
        };
        let _ = recursive;

        if query.trim().is_empty() {
            self.views.clear();
            self.visible_entries.borrow_mut().clear();
            self.spinner.stop();
            self.spinner.set_visible(false);
            self.update_status();
            return;
        }
        let Some(root_path) = loc.local_path().map(PathBuf::from) else {
            return;
        };
        let hidden = self.config.borrow().show_hidden;

        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                glib::timeout_future(Duration::from_millis(160)).await;
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let still_valid = this
                    .tabs
                    .borrow()
                    .iter()
                    .find(|t| t.id == tab_id)
                    .is_some_and(|t| t.search_revision == revision && t.recursive);
                if !still_valid {
                    return;
                }
                if let Some(tab) = this.tabs.borrow_mut().iter_mut().find(|t| t.id == tab_id) {
                    tab.loading = true;
                    tab.all_entries.clear();
                }
                if this.active_tab_id() == tab_id {
                    this.views.clear();
                    this.visible_entries.borrow_mut().clear();
                    this.spinner.set_visible(true);
                    this.spinner.start();
                    this.update_status();
                }

                let cancel_batch = cancel.clone();
                let cancel_done = cancel.clone();
                search::spawn(
                    root_path,
                    query,
                    hidden,
                    cancel,
                    glib::clone!(
                        #[weak]
                        this,
                        move |batch| {
                            if cancel_batch.load(Ordering::Relaxed) {
                                return;
                            }
                            let mut appended_active = false;
                            if let Some(tab) = this
                                .tabs
                                .borrow_mut()
                                .iter_mut()
                                .find(|t| t.id == tab_id && t.search_revision == revision && t.recursive)
                            {
                                tab.all_entries.extend(batch.clone());
                                appended_active = this.active_tab_id() == tab_id;
                            }
                            if appended_active {
                                this.visible_entries.borrow_mut().extend(batch.clone());
                                this.views.append_entries(&batch);
                                this.update_status();
                            }
                        }
                    ),
                    glib::clone!(
                        #[weak]
                        this,
                        move |res| {
                            let mut finished_current = false;
                            if let Some(tab) = this
                                .tabs
                                .borrow_mut()
                                .iter_mut()
                                .find(|t| t.id == tab_id && t.search_revision == revision && t.recursive)
                            {
                                tab.loading = false;
                                finished_current = true;
                            }
                            if finished_current
                                && !cancel_done.load(Ordering::Relaxed)
                                && this.active_tab_id() == tab_id
                            {
                                this.spinner.stop();
                                this.spinner.set_visible(false);
                                this.update_status();
                                match res {
                                    Ok(true) => this.notify(
                                        "Search stopped at 10,000 results. Refine your query.",
                                    ),
                                    Ok(false) => {}
                                    Err(e) if e != "Cancelled" => this.notify(&e),
                                    Err(_) => {}
                                }
                            }
                        }
                    ),
                );
            }
        ));
    }

    fn activate(self: &Rc<Self>, position: u32) {
        if let Some(entry) = self.views.entry_at(position) {
            self.open_item(entry);
        }
    }

    fn open_selected(self: &Rc<Self>) {
        let selected = self.views.selected();
        if selected.len() == 1 {
            self.open_item(selected.into_iter().next().unwrap());
            return;
        }
        for entry in selected {
            if !entry.is_dir {
                self.open_item(entry);
            }
        }
    }

    fn open_item(self: &Rc<Self>, entry: FileEntry) {
        if self.is_trash() || entry.trash.is_some() {
            self.quicklook();
            return;
        }
        if entry.is_dir {
            self.navigate(Location::Local(entry.path), true);
            return;
        }
        let cfg = self.config.borrow().clone();
        open::open_entry(&self.window, entry, cfg, self.reporter());
    }

    fn open_with(self: &Rc<Self>) {
        if let Some(entry) = self.views.selected().into_iter().next() {
            if entry.is_dir {
                return;
            }
            let cfg = self.config.borrow().clone();
            open::open_with_dialog(&self.window, entry, None, cfg, self.reporter());
        }
    }

    fn middle_click_item(self: &Rc<Self>, position: u32) {
        if let Some(entry) = self.views.entry_at(position) {
            if entry.is_dir && entry.trash.is_none() {
                self.new_tab(Location::Local(entry.path), true);
            }
        }
    }

    fn quicklook(self: &Rc<Self>) {
        let Some(selected) = self.views.selected().into_iter().next() else {
            return;
        };
        let entries = self.visible_entries.borrow().clone();
        let idx = entries
            .iter()
            .position(|e| e.uri == selected.uri)
            .unwrap_or(0);
        let views_grid = self.views.grid.clone();
        let views_list = self.views.list.clone();
        let mode = self.views.mode();
        preview::show_quicklook(
            &self.window,
            entries,
            idx,
            self.reporter(),
            Rc::new(move || match mode {
                ViewMode::Grid => {
                    views_grid.grab_focus();
                }
                ViewMode::List => {
                    views_list.grab_focus();
                }
            }),
        );
    }

    fn reload_tab_after_op(self: &Rc<Self>, tab_id: u64) {
        if self.active_tab_id() == tab_id {
            self.reload(true);
        } else if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|t| t.id == tab_id) {
            tab.needs_refresh = true;
        }
    }

    fn finish_tab_mutation(self: &Rc<Self>, tab_id: u64, created_paths: Vec<PathBuf>) {
        if !created_paths.is_empty() {
            let uris: Vec<String> = created_paths
                .iter()
                .map(|p| model::file_uri_from_path(p))
                .collect();
            if let Some(tab) = self.tabs.borrow_mut().iter_mut().find(|t| t.id == tab_id) {
                tab.pending_selection = uris;
            }
        }
        self.reload_tab_after_op(tab_id);
    }

    fn refresh_location(self: &Rc<Self>, loc: &Location) {
        let active_id = self.active_tab_id();
        let mut reload_active = false;
        for tab in self.tabs.borrow_mut().iter_mut() {
            if &tab.location == loc {
                if tab.id == active_id {
                    reload_active = true;
                } else {
                    tab.needs_refresh = true;
                }
            }
        }
        if reload_active {
            self.reload(true);
        }
    }

    fn properties(self: &Rc<Self>) {
        let entry = if let Some(sel) = self.views.selected().into_iter().next() {
            sel
        } else if let Some(path) = self.current_location().local_path() {
            let Some(cur) = model::entry_from_path(path, self.root.borrow().is_some()) else {
                return;
            };
            cur
        } else {
            return;
        };

        let is_root = self.root.borrow().is_some();
        let can_chmod = is_root || !entry.is_readonly;
        preview::show_properties(
            &self.window,
            entry,
            &self.icon_cache,
            can_chmod,
            Rc::new(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |path, mode, done| this.apply_chmod(path, mode, done)
            )),
        );
    }

    fn apply_chmod(self: &Rc<Self>, path: PathBuf, mode: u32, done: Box<dyn FnOnce(bool)>) {
        let tab_id = self.active_tab_id();
        if let Some(root) = self.root.borrow().clone() {
            self.touch_root();
            glib::spawn_future_local(glib::clone!(
                #[weak(rename_to = this)]
                self,
                async move {
                    let ok = match root.run(root_proto::Request::Chmod { path, mode }).await {
                        Ok(()) => true,
                        Err(e) => {
                            this.notify(&e);
                            false
                        }
                    };
                    done(ok);
                    if ok {
                        this.reload_tab_after_op(tab_id);
                    }
                }
            ));
            return;
        }
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                let ok = match ops::chmod_path(path, mode).await {
                    Ok(()) => true,
                    Err(e) => {
                        this.notify(&e);
                        false
                    }
                };
                done(ok);
                if ok {
                    this.reload_tab_after_op(tab_id);
                }
            }
        ));
    }

    fn start_rename(&self) {
        let idx = self.active_tab.get();
        let siblings: HashSet<String> = self
            .tabs
            .borrow()
            .get(idx)
            .map(|t| t.all_entries.iter().map(|e| e.name.clone()).collect())
            .unwrap_or_default();
        self.views.open_rename_popover(siblings);
    }

    fn rename_entry(self: &Rc<Self>, entry: FileEntry, new_name: String) {
        if self.is_trash() || entry.trash.is_some() {
            self.notify("Restore files before renaming them");
            return;
        }
        if new_name == entry.name {
            return;
        }
        if new_name.is_empty()
            || new_name == "."
            || new_name == ".."
            || new_name.contains(['/', '\0'])
        {
            self.notify("Invalid name");
            return;
        }
        let Some(parent) = entry.path.parent() else {
            return;
        };
        let to = parent.join(&new_name);
        if self.root_run(
            root_proto::Request::Rename {
                from: entry.path.clone(),
                to: to.clone(),
                overwrite: false,
            },
            vec![to],
        ) {
            return;
        }
        let tab_id = self.active_tab_id();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                match ops::rename_path(entry.path, new_name).await {
                    Ok(new_path) => this.finish_tab_mutation(tab_id, vec![new_path]),
                    Err(e) => this.notify(&e),
                }
            }
        ));
    }

    fn new_folder(self: &Rc<Self>) {
        self.prompt_new_entry(true);
    }

    fn new_file(self: &Rc<Self>) {
        self.prompt_new_entry(false);
    }

    fn prompt_new_entry(self: &Rc<Self>, folder: bool) {
        let Some(dir) = self.current_location().local_path().map(PathBuf::from) else {
            self.notify("Cannot create items in Trash");
            return;
        };
        let heading = if folder { "New Folder" } else { "New File" };
        let default_name = if folder { "New Folder" } else { "untitled.txt" };
        let dialog = adw::AlertDialog::builder().heading(heading).build();
        let name_entry = gtk::Entry::builder()
            .text(default_name)
            .activates_default(true)
            .build();
        dialog.set_extra_child(Some(&name_entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_focus(Some(&name_entry));
        if !folder {
            name_entry.select_region(0, default_name.rfind('.').map(|i| i as i32).unwrap_or(-1));
        } else {
            name_entry.select_region(0, -1);
        }

        let tab_id = self.active_tab_id();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                if dialog.choose_future(Some(&this.window)).await != "create" {
                    return;
                }
                let name = name_entry.text().to_string();
                if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
                    this.notify("Invalid name");
                    return;
                }
                let target = dir.join(&name);
                let req = if folder {
                    root_proto::Request::Mkdir {
                        path: target.clone(),
                    }
                } else {
                    root_proto::Request::Touch {
                        path: target.clone(),
                    }
                };
                if this.root_run(req, vec![target]) {
                    return;
                }
                let res = if folder {
                    ops::create_folder(dir, name).await
                } else {
                    ops::create_file(dir, name).await
                };
                match res {
                    Ok(created) => this.finish_tab_mutation(tab_id, vec![created]),
                    Err(e) => this.notify(&e),
                }
            }
        ));
    }

    fn copy(&self, cut: bool) {
        if self.is_trash() {
            self.notify("Restore files before copying or cutting them");
            return;
        }
        let uris: Vec<String> = self.views.selected().into_iter().map(|e| e.uri).collect();
        if uris.is_empty() {
            return;
        }
        if cut {
            self.views.set_cut_uris(uris.iter().cloned().collect());
        } else {
            self.views.set_cut_uris(HashSet::new());
        }
        let files = clipboard::Files { cut, uris };
        if let Err(e) = self.window.clipboard().set_content(Some(&files.provider())) {
            self.notify(&e.to_string());
        } else {
            self.notify(if cut { "Files cut" } else { "Files copied" });
        }
    }

    fn duplicate(self: &Rc<Self>) {
        if self.is_trash() {
            return;
        }
        let selected = self.views.selected();
        if selected.is_empty() {
            return;
        }
        let tab_id = self.active_tab_id();
        if let Some(root) = self.root.borrow().clone() {
            let mut taken: HashSet<String> = self
                .tabs
                .borrow()
                .iter()
                .find(|t| t.id == tab_id)
                .map(|t| t.all_entries.iter().map(|e| e.name.clone()).collect())
                .unwrap_or_default();
            let op = self.operations.start("Duplicating");
            self.touch_root();
            let _hold = self.app.hold();
            glib::spawn_future_local(glib::clone!(
                #[strong(rename_to = this)]
                self,
                async move {
                    let _hold = _hold;
                    let mut created = Vec::new();
                    for entry in selected {
                        if op.cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let Some(parent) = entry.path.parent() else {
                            continue;
                        };
                        let name = next_copy_name(&entry.name, entry.is_dir, true, &mut taken);
                        let to = parent.join(name);
                        match root
                            .run(root_proto::Request::Copy {
                                from: entry.path,
                                to: to.clone(),
                                overwrite: false,
                            })
                            .await
                        {
                            Ok(()) => created.push(to),
                            Err(e) => {
                                this.notify(&e);
                                break;
                            }
                        }
                    }
                    if !created.is_empty() {
                        this.notify(&format!("Duplicated {} item(s)", created.len()));
                    }
                    op.finish();
                    this.finish_tab_mutation(tab_id, created);
                }
            ));
            return;
        }
        let paths: Vec<PathBuf> = selected.into_iter().map(|e| e.path).collect();
        let op = self.operations.start("Duplicating");
        let cancel = op.cancel.clone();
        let _hold = self.app.hold();
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to = this)]
            self,
            async move {
                let _hold = _hold;
                let mut created_paths = Vec::new();
                match gio::spawn_blocking(move || ops::duplicate_sync(&paths, &cancel)).await {
                    Ok(Ok(created)) => {
                        this.notify(&format!("Duplicated {} item(s)", created.len()));
                        created_paths = created;
                    }
                    Ok(Err(e)) => this.notify(&e),
                    Err(_) => this.notify("Duplicate worker failed"),
                }
                op.finish();
                this.finish_tab_mutation(tab_id, created_paths);
            }
        ));
    }

    fn paste(self: &Rc<Self>) {
        let Some(dest) = self.current_location().local_path().map(PathBuf::from) else {
            self.notify("Files cannot be pasted into Trash");
            return;
        };
        let clipboard = self.window.clipboard();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                match clipboard::read(&clipboard).await {
                    Ok(files) => this.transfer_files(files, dest, true, None),
                    Err(e) => this.notify(&e),
                }
            }
        ));
    }

    fn drag_uris_for(&self, pos: u32) -> Vec<String> {
        if self.is_trash() {
            return Vec::new();
        }
        if !self.views.selection.is_selected(pos) {
            self.views.selection.select_item(pos, true);
        }
        self.views.selected().into_iter().map(|e| e.uri).collect()
    }

    fn handle_drop(
        self: &Rc<Self>,
        target_loc: Option<Location>,
        uris: Vec<String>,
        moving: bool,
        drop: gdk::Drop,
    ) {
        let is_bg = target_loc.is_none();
        let target = target_loc.unwrap_or_else(|| self.current_location());
        match target {
            Location::Trash => {
                if is_bg {
                    drop.finish(gdk::DragAction::empty());
                    return;
                }
                let paths: Vec<PathBuf> = uris
                    .into_iter()
                    .filter_map(|u| Location::from_uri(&u).ok()?.local_path().map(PathBuf::from))
                    .collect();
                if paths.is_empty() {
                    drop.finish(gdk::DragAction::empty());
                    return;
                }
                self.trash_paths(paths, Some(drop));
            }
            Location::Local(dir) => {
                let files = clipboard::Files { cut: moving, uris };
                self.transfer_files(files, dir, false, Some(drop));
            }
        }
    }

    fn transfer_files(
        self: &Rc<Self>,
        files: clipboard::Files,
        destination: PathBuf,
        clear_clipboard: bool,
        drop: Option<gdk::Drop>,
    ) {
        if let Some(root) = self.root.borrow().clone() {
            self.root_transfer(root, files, destination, clear_clipboard, drop);
            return;
        }
        let sources: Vec<PathBuf> = files
            .uris
            .iter()
            .filter_map(|u| Location::from_uri(u).ok()?.local_path().map(PathBuf::from))
            .collect();
        if sources.is_empty() {
            if let Some(drop) = drop {
                drop.finish(gdk::DragAction::empty());
            }
            return;
        }
        let op = self.operations.start(if files.cut {
            "Moving files"
        } else {
            "Copying files"
        });
        let tab_id = self.active_tab_id();
        let _hold = self.app.hold();
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to = this)]
            self,
            async move {
                let _hold = _hold;
                let mut success = false;
                let dest_loc = Location::Local(destination.clone());
                match ops::transfer(&this.window, sources, destination, files.cut, &op).await {
                    Ok(report) => {
                        success = !report.cancelled;
                        if files.cut
                            && clear_clipboard
                            && !report.cancelled
                            && report.skipped == 0
                        {
                            this.views.set_cut_uris(HashSet::new());
                            let cb = this.window.clipboard();
                            if clipboard::read(&cb)
                                .await
                                .is_ok_and(|cur| cur == files)
                            {
                                let _ = cb.set_content(None::<&gdk::ContentProvider>);
                            }
                        }
                        if !report.created_paths.is_empty() {
                            let uris: Vec<String> = report
                                .created_paths
                                .iter()
                                .map(|p| model::file_uri_from_path(p))
                                .collect();
                            if let Some(tab) = this
                                .tabs
                                .borrow_mut()
                                .iter_mut()
                                .find(|t| t.id == tab_id && t.location == dest_loc)
                            {
                                tab.pending_selection = uris;
                            }
                        }
                        if report.cancelled {
                            this.notify("Transfer cancelled");
                        } else {
                            this.notify(&format!("{} items completed", report.completed));
                        }
                    }
                    Err(e) => this.notify(&e),
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
                this.reload_tab_after_op(tab_id);
                this.refresh_location(&dest_loc);
            }
        ));
    }

    fn delete(self: &Rc<Self>, permanent: bool) {
        let selected = self.views.selected();
        if selected.is_empty() {
            return;
        }
        if self.is_trash() {
            self.delete_from_trash(selected);
            return;
        }
        if self.root.borrow().is_some() {
            self.root_delete(selected);
            return;
        }
        let paths: Vec<PathBuf> = selected.into_iter().map(|e| e.path).collect();
        if permanent {
            glib::spawn_future_local(glib::clone!(
                #[strong(rename_to = this)]
                self,
                async move {
                    if !ops::confirm(
                        &this.window,
                        "Delete permanently?",
                        &format!(
                            "{} selected item(s) will be permanently deleted. This cannot be undone.",
                            paths.len()
                        ),
                        "Delete Permanently",
                    )
                    .await
                    {
                        return;
                    }
                    let _hold = this.app.hold();
                    let op = this.operations.start("Deleting files");
                    match ops::delete_paths(paths, &op).await {
                        Ok(n) => this.notify(&format!("Deleted {n} item(s)")),
                        Err(e) => this.notify(&e),
                    }
                    op.finish();
                    this.reload(true);
                }
            ));
        } else {
            self.trash_paths(paths, None);
        }
    }

    fn trash_paths(self: &Rc<Self>, paths: Vec<PathBuf>, drop: Option<gdk::Drop>) {
        let op = self.operations.start("Moving to Trash");
        let _hold = self.app.hold();
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to = this)]
            self,
            async move {
                let _hold = _hold;
                let count = paths.len();
                let res = gio::spawn_blocking(move || {
                    let mut trashed = Vec::with_capacity(paths.len());
                    for p in paths {
                        trashed.push(trash::move_to_trash(&p)?);
                    }
                    Ok::<_, String>(trashed)
                })
                .await;
                op.finish();
                let mut ok = false;
                match res {
                    Ok(Ok(metas)) if !metas.is_empty() => {
                        ok = true;
                        this.sidebar.set_trash_count(trash::trash_count());
                        let toast = adw::Toast::builder()
                            .title(format!("{count} item(s) moved to Trash"))
                            .button_label("Undo")
                            .timeout(12)
                            .build();
                        toast.connect_button_clicked(glib::clone!(
                            #[weak]
                            this,
                            move |_| this.restore_metas(metas.clone())
                        ));
                        this.toast.add_toast(toast);
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => this.notify(&e),
                    Err(_) => this.notify("Trash worker failed"),
                }
                if let Some(drop) = drop {
                    drop.finish(if ok {
                        gdk::DragAction::MOVE
                    } else {
                        gdk::DragAction::empty()
                    });
                }
                this.reload(true);
            }
        ));
    }

    fn delete_from_trash(self: &Rc<Self>, selected: Vec<FileEntry>) {
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to = this)]
            self,
            async move {
                if !ops::confirm(
                    &this.window,
                    "Delete from Trash permanently?",
                    &format!(
                        "{} selected item(s) will be permanently deleted.",
                        selected.len()
                    ),
                    "Delete Permanently",
                )
                .await
                {
                    return;
                }
                let names: Vec<std::ffi::OsString> = selected
                    .into_iter()
                    .filter_map(|e| e.trash.map(|m| m.trash_name))
                    .collect();
                let res = gio::spawn_blocking(move || {
                    for name in &names {
                        trash::delete_trashed(name)?;
                    }
                    Ok::<_, String>(names.len())
                })
                .await;
                match res {
                    Ok(Ok(n)) => this.notify(&format!("Deleted {n} item(s)")),
                    Ok(Err(e)) => this.notify(&e),
                    Err(_) => this.notify("Trash worker failed"),
                }
                this.sidebar.set_trash_count(trash::trash_count());
                this.reload(true);
            }
        ));
    }

    fn restore_selected(self: &Rc<Self>) {
        let metas: Vec<model::TrashMeta> = self
            .views
            .selected()
            .into_iter()
            .filter_map(|e| e.trash)
            .collect();
        if !metas.is_empty() {
            self.restore_metas(metas);
        }
    }

    fn restore_metas(self: &Rc<Self>, metas: Vec<model::TrashMeta>) {
        let op = self.operations.start("Restoring files");
        let _hold = self.app.hold();
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to = this)]
            self,
            async move {
                let _hold = _hold;
                let mut restored = 0usize;
                for meta in metas {
                    let target = if std::fs::symlink_metadata(&meta.orig_path).is_ok() {
                        let parent = meta
                            .orig_path
                            .parent()
                            .unwrap_or_else(|| std::path::Path::new("/"));
                        let name = meta
                            .orig_path
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| meta.trash_name.to_string_lossy().into_owned());
                        match ops::unique_copy_target(parent, &name, false) {
                            Ok(u) => u,
                            Err(e) => {
                                this.notify(&e);
                                continue;
                            }
                        }
                    } else {
                        meta.orig_path.clone()
                    };
                    let m = meta.clone();
                    match gio::spawn_blocking(move || trash::restore_item(&m, &target)).await {
                        Ok(Ok(())) => restored += 1,
                        Ok(Err(e)) => this.notify(&e),
                        Err(_) => this.notify("Restore worker failed"),
                    }
                }
                op.finish();
                this.sidebar.set_trash_count(trash::trash_count());
                if restored > 0 {
                    this.notify(&format!("Restored {restored} item(s)"));
                }
                this.reload(true);
            }
        ));
    }

    fn empty_trash(self: &Rc<Self>) {
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to = this)]
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
                match gio::spawn_blocking(trash::empty_trash).await {
                    Ok(Ok(n)) => this.notify(&format!("Emptied Trash ({n} items)")),
                    Ok(Err(e)) => this.notify(&e),
                    Err(_) => this.notify("Trash worker failed"),
                }
                op.finish();
                this.sidebar.set_trash_count(0);
                this.reload(true);
            }
        ));
    }

    fn extract_selected(self: &Rc<Self>) {
        let Some(entry) = self.views.selected().into_iter().next() else {
            return;
        };
        let Some(dest_dir) = self.current_location().local_path().map(PathBuf::from) else {
            return;
        };
        let op = self.operations.start("Extracting archive");
        let tab_id = self.active_tab_id();
        let _hold = self.app.hold();
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to = this)]
            self,
            async move {
                let _hold = _hold;
                let mut created = Vec::new();
                match ops::extract_archive(entry.path, dest_dir, &op).await {
                    Ok(out) => {
                        this.notify(&format!(
                            "Extracted to {}",
                            out.file_name().unwrap_or_default().to_string_lossy()
                        ));
                        created.push(out);
                    }
                    Err(e) => this.notify(&e),
                }
                op.finish();
                this.finish_tab_mutation(tab_id, created);
            }
        ));
    }

    fn compress_selected(self: &Rc<Self>) {
        let selected = self.views.selected();
        if selected.is_empty() {
            return;
        }
        let Some(dir) = self.current_location().local_path().map(PathBuf::from) else {
            return;
        };
        let default_base = if selected.len() == 1 {
            selected[0].name.clone()
        } else {
            "archive".to_owned()
        };
        let dialog = adw::AlertDialog::builder()
            .heading("Compress")
            .body("Choose archive filename (.tar.gz, .tar.zst, or .zip):")
            .build();
        let entry = gtk::Entry::builder()
            .text(format!("{default_base}.tar.gz"))
            .activates_default(true)
            .build();
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("create", "Compress")]);
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_focus(Some(&entry));
        entry.select_region(0, default_base.chars().count() as i32);

        let tab_id = self.active_tab_id();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                if dialog.choose_future(Some(&this.window)).await != "create" {
                    return;
                }
                let name = entry.text().to_string();
                if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
                    this.notify("Invalid archive name");
                    return;
                }
                let archive_path = dir.join(&name);
                let sources: Vec<PathBuf> = selected.into_iter().map(|e| e.path).collect();
                let op = this.operations.start("Compressing");
                let _hold = this.app.hold();
                let mut created = Vec::new();
                match ops::compress_paths(sources, archive_path, &op).await {
                    Ok(out) => {
                        this.notify("Archive created");
                        created.push(out);
                    }
                    Err(e) => this.notify(&e),
                }
                op.finish();
                this.finish_tab_mutation(tab_id, created);
            }
        ));
    }

    fn copy_selected_paths(&self) {
        let selected = self.views.selected();
        let text = if selected.is_empty() {
            self.current_location().parse_name()
        } else {
            selected
                .iter()
                .map(|e| e.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.views.set_cut_uris(HashSet::new());
        clipboard::copy_text(&self.window.clipboard(), &text);
        self.notify("Path copied");
    }

    fn toggle_bookmark_for_target(&self) {
        let selected = self.views.selected();
        let target = if selected.len() == 1 && selected[0].is_dir {
            Some(selected[0].path.clone())
        } else {
            self.current_location().local_path().map(PathBuf::from)
        };
        let Some(path) = target else {
            return;
        };
        if self.sidebar.is_bookmarked(&path) {
            self.sidebar
                .remove_bookmark(&model::file_uri_from_path(&path));
            self.notify("Bookmark removed");
        } else {
            self.sidebar.add_bookmark(&path);
            self.notify("Bookmark added");
        }
    }

    fn terminal(&self) {
        let selected = self.views.selected();
        let folder = selected
            .first()
            .filter(|e| e.is_dir && e.trash.is_none())
            .map(|e| e.path.clone())
            .or_else(|| self.current_location().local_path().map(PathBuf::from));
        let Some(path) = folder else {
            self.notify("Open in Terminal requires a local folder");
            return;
        };
        self.external_terminal(path);
    }

    fn external_terminal(&self, path: PathBuf) {
        let cmd = self.config.borrow().terminal_command();
        if let Err(e) = open::spawn_terminal_window(&cmd, &path) {
            self.notify(&e);
        }
    }

    fn dock_folder_target(&self) -> Option<PathBuf> {
        self.views
            .selected()
            .first()
            .filter(|e| e.is_dir && e.trash.is_none())
            .map(|e| e.path.clone())
            .or_else(|| self.current_location().local_path().map(PathBuf::from))
    }

    fn toggle_dock(self: &Rc<Self>) {
        if self.dock.is_open() {
            self.dock.hide();
            self.views.grab_focus();
            return;
        }
        self.open_dock_here();
    }

    fn open_dock_here(self: &Rc<Self>) {
        match self.dock_folder_target() {
            Some(path) => self.dock.open(&path),
            None => self.notify("The terminal needs a local folder"),
        }
    }

    fn mark_other_tabs_stale(&self) {
        let active_id = self.active_tab_id();
        for tab in self.tabs.borrow_mut().iter_mut() {
            if tab.id != active_id {
                tab.needs_refresh = true;
            }
        }
    }

    fn toggle_root(self: &Rc<Self>) {
        if self.root.borrow().is_some() {
            self.leave_root(None);
            return;
        }
        self.root_button.set_sensitive(false);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                let result = root::RootSession::start(&this.app).await;
                this.root_button.set_sensitive(true);
                match result {
                    Ok(session) => {
                        this.root.replace(Some(Rc::new(session)));
                        this.window.add_css_class("is-root");
                        this.root_button.add_css_class("on");
                        this.root_bar.set_reveal_child(true);
                        this.who.set_text("root");
                        this.who.add_css_class("root");
                        this.mark_other_tabs_stale();
                        this.reload(false);
                    }
                    Err(e) => this.notify(&e),
                }
            }
        ));
    }

    fn leave_root(self: &Rc<Self>, reason: Option<&str>) {
        if self.root.borrow_mut().take().is_none() {
            return;
        }
        if let Some(id) = self.root_idle.borrow_mut().take() {
            id.remove();
        }
        self.window.remove_css_class("is-root");
        self.root_button.remove_css_class("on");
        self.root_bar.set_reveal_child(false);
        self.who.set_text(&glib::user_name().to_string_lossy());
        self.who.remove_css_class("root");
        if let Some(reason) = reason {
            self.notify(reason);
        }
        self.mark_other_tabs_stale();
        self.reload(false);
    }

    fn touch_root(self: &Rc<Self>) {
        if let Some(id) = self.root_idle.borrow_mut().take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_seconds_local_once(300, move || {
            if let Some(this) = weak.upgrade() {
                this.root_idle.borrow_mut().take();
                this.leave_root(Some("Root mode ended after 5 minutes without changes"));
            }
        });
        self.root_idle.replace(Some(id));
    }

    fn root_run(self: &Rc<Self>, request: root_proto::Request, select: Vec<PathBuf>) -> bool {
        let Some(root) = self.root.borrow().clone() else {
            return false;
        };
        let tab_id = self.active_tab_id();
        self.touch_root();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                match root.run(request).await {
                    Ok(()) => this.finish_tab_mutation(tab_id, select),
                    Err(e) => {
                        this.notify(&e);
                        this.reload_tab_after_op(tab_id);
                    }
                }
            }
        ));
        true
    }

    fn root_delete(self: &Rc<Self>, selected: Vec<FileEntry>) {
        let Some(root) = self.root.borrow().clone() else {
            return;
        };
        let tab_id = self.active_tab_id();
        self.touch_root();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                if !ops::confirm(
                    &this.window,
                    "Delete permanently as root?",
                    &format!(
                        "{} selected items will be permanently deleted. Root mode has no Trash.",
                        selected.len()
                    ),
                    "Delete Permanently",
                )
                .await
                {
                    return;
                }
                for entry in selected {
                    if let Err(e) = root
                        .run(root_proto::Request::Delete { path: entry.path })
                        .await
                    {
                        this.notify(&e);
                        break;
                    }
                }
                this.reload_tab_after_op(tab_id);
            }
        ));
    }

    fn root_transfer(
        self: &Rc<Self>,
        root: Rc<root::RootSession>,
        files: clipboard::Files,
        dir: PathBuf,
        clear_clipboard: bool,
        drop: Option<gdk::Drop>,
    ) {
        let sources: Vec<PathBuf> = files
            .uris
            .iter()
            .filter_map(|u| Location::from_uri(u).ok()?.local_path().map(PathBuf::from))
            .collect();
        if sources.is_empty() {
            if let Some(drop) = drop {
                drop.finish(gdk::DragAction::empty());
            }
            return;
        }
        let known_dirs: HashSet<PathBuf> = self
            .tabs
            .borrow()
            .iter()
            .flat_map(|t| {
                t.all_entries
                    .iter()
                    .filter(|e| e.is_dir)
                    .map(|e| e.path.clone())
            })
            .collect();
        let op = self.operations.start(if files.cut {
            "Moving files"
        } else {
            "Copying files"
        });
        self.touch_root();
        let tab_id = self.active_tab_id();
        let _hold = self.app.hold();
        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to = this)]
            self,
            async move {
                let _hold = _hold;
                let mut taken: HashSet<String> = if !files.cut {
                    root.list(&dir)
                        .await
                        .map(|entries| entries.into_iter().map(|e| e.name).collect())
                        .unwrap_or_default()
                } else {
                    HashSet::new()
                };
                let mut created = Vec::new();
                let mut had_error = false;
                for from in sources {
                    if op.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some(raw_name) = from.file_name().map(|s| s.to_string_lossy().into_owned())
                    else {
                        continue;
                    };
                    let to = if files.cut {
                        dir.join(&raw_name)
                    } else {
                        let is_dir = known_dirs.contains(&from)
                            || std::fs::symlink_metadata(&from)
                                .map(|m| m.is_dir())
                                .unwrap_or(false);
                        dir.join(next_copy_name(&raw_name, is_dir, false, &mut taken))
                    };
                    let request = if files.cut {
                        root_proto::Request::Move {
                            from,
                            to: to.clone(),
                            overwrite: false,
                        }
                    } else {
                        root_proto::Request::Copy {
                            from,
                            to: to.clone(),
                            overwrite: false,
                        }
                    };
                    if let Err(e) = root.run(request).await {
                        this.notify(&e);
                        had_error = true;
                        break;
                    }
                    created.push(to);
                }
                let cancelled = op.cancel.load(Ordering::Relaxed);
                let success = !cancelled && !had_error;
                if files.cut && clear_clipboard && success {
                    this.views.set_cut_uris(HashSet::new());
                    let cb = this.window.clipboard();
                    if clipboard::read(&cb).await.is_ok_and(|cur| cur == files) {
                        let _ = cb.set_content(None::<&gdk::ContentProvider>);
                    }
                }
                let dest_loc = Location::Local(dir);
                if !created.is_empty() {
                    let uris: Vec<String> = created
                        .iter()
                        .map(|p| model::file_uri_from_path(p))
                        .collect();
                    if let Some(tab) = this
                        .tabs
                        .borrow_mut()
                        .iter_mut()
                        .find(|t| t.id == tab_id && t.location == dest_loc)
                    {
                        tab.pending_selection = uris;
                    }
                }
                if cancelled {
                    this.notify("Transfer cancelled");
                } else if !created.is_empty() {
                    this.notify(&format!("{} items completed", created.len()));
                }
                if let Some(drop) = drop {
                    drop.finish(if success && !created.is_empty() {
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
                this.reload_tab_after_op(tab_id);
                this.refresh_location(&dest_loc);
            }
        ));
    }

    fn track_and_popup(self: &Rc<Self>, popover: gtk::Popover) {
        self.close_active_popover();
        let tracked = popover.clone();
        popover.connect_closed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |p| {
                if this.active_popover.borrow().as_ref() == Some(&tracked) {
                    this.active_popover.borrow_mut().take();
                }
                p.unparent();
            }
        ));
        self.active_popover.replace(Some(popover.clone()));
        popover.popup();
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
        let Ok(Location::Local(folder)) = self.resolve_path(&base) else {
            return;
        };
        let revision = self.completion_revision.get() + 1;
        self.completion_revision.set(revision);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                let folder_scan = folder.clone();
                let prefix_lc = prefix.to_lowercase();
                let Ok(Ok(mut names)) = gio::spawn_blocking(move || {
                    let rd = std::fs::read_dir(&folder_scan).map_err(|e| e.to_string())?;
                    let mut out = Vec::new();
                    for item in rd.flatten() {
                        let is_dir = item
                            .file_type()
                            .map(|t| t.is_dir())
                            .unwrap_or(false)
                            || item.path().is_dir();
                        if !is_dir {
                            continue;
                        }
                        let name = item.file_name().to_string_lossy().into_owned();
                        if name.to_lowercase().starts_with(&prefix_lc) {
                            out.push(name);
                        }
                    }
                    out.sort_by(|a, b| model::natural_cmp(a, b));
                    Ok::<_, String>(out)
                })
                .await
                else {
                    return;
                };

                if this.completion_revision.get() != revision
                    || this.path.text() != text
                    || this.path_stack.visible_child_name().as_deref() != Some("entry")
                {
                    return;
                }
                if names.len() == 1 {
                    let target = folder.join(names.remove(0));
                    this.path.set_text(&format!(
                        "{}/",
                        target.to_string_lossy().trim_end_matches('/')
                    ));
                    this.path.set_position(-1);
                } else if !names.is_empty() {
                    let common: String = names.iter().skip(1).fold(names[0].clone(), |acc, name| {
                        acc.chars()
                            .zip(name.chars())
                            .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
                            .map(|(a, _)| a)
                            .collect()
                    });
                    if common.chars().count() > prefix.chars().count() {
                        let target = folder.join(common);
                        this.path.set_text(&target.to_string_lossy());
                        this.path.set_position(-1);
                    }
                    let popover = gtk::Popover::new();
                    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
                    for name in names.iter().take(40) {
                        let button = gtk::Button::with_label(name);
                        button.add_css_class("flat");
                        if let Some(lbl) = button.child().and_downcast::<gtk::Label>() {
                            lbl.set_xalign(0.);
                        }
                        let target = folder.join(name);
                        button.connect_clicked(glib::clone!(
                            #[weak]
                            this,
                            #[weak]
                            popover,
                            move |_| {
                                this.path.set_text(&format!(
                                    "{}/",
                                    target.to_string_lossy().trim_end_matches('/')
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
                    this.track_and_popup(popover);
                }
            }
        ));
    }

    fn handle_context_menu(self: &Rc<Self>, pos: Option<u32>, x: f64, y: f64) {
        if let Some(p) = pos {
            if !self.views.selection.is_selected(p) {
                self.views.selection.select_item(p, true);
            }
        } else {
            self.views.selection.unselect_all();
        }
        self.show_context_menu(x, y);
    }

    fn show_context_menu(self: &Rc<Self>, x: f64, y: f64) {
        let menu = gio::Menu::new();
        let selected = self.views.selected();
        if self.is_trash() {
            if !selected.is_empty() {
                menu.append(Some("Restore"), Some("win.restore"));
                if selected.len() == 1 {
                    menu.append(Some("Quick Look"), Some("win.quicklook"));
                }
                menu.append(Some("Delete Permanently…"), Some("win.delete-permanently"));
            }
            menu.append(Some("Empty Trash…"), Some("win.empty-trash"));
        } else if selected.is_empty() {
            let create = gio::Menu::new();
            create.append(Some("New Folder…"), Some("win.new-folder"));
            create.append(Some("New File…"), Some("win.new-file"));
            create.append(Some("Paste"), Some("win.paste"));
            menu.append_section(None, &create);

            let term = gio::Menu::new();
            term.append(Some("Terminal Here"), Some("win.dock-here"));
            term.append(Some("Terminal in New Window"), Some("win.terminal"));
            menu.append_section(None, &term);

            let view = gio::Menu::new();
            let sort_sub = gio::Menu::new();
            sort_sub.append(Some("Name"), Some("win.sort-name"));
            sort_sub.append(Some("Size"), Some("win.sort-size"));
            sort_sub.append(Some("Modified"), Some("win.sort-modified"));
            sort_sub.append(Some("Type"), Some("win.sort-type"));
            sort_sub.append(Some("Descending"), Some("win.sort-desc"));
            view.append_submenu(Some("Sort By"), &sort_sub);
            view.append(Some("Show Hidden Files"), Some("win.hidden"));
            if let Some(p) = self.current_location().local_path() {
                let bm_label = if self.sidebar.is_bookmarked(p) {
                    "Remove Bookmark"
                } else {
                    "Bookmark This Folder"
                };
                view.append(Some(bm_label), Some("win.bookmark"));
            }
            view.append(Some("Select All"), Some("win.select-all"));
            view.append(Some("Refresh"), Some("win.refresh"));
            view.append(Some("Copy Path"), Some("win.copy-path"));
            menu.append_section(None, &view);
            menu.append(Some("Properties"), Some("win.properties"));
        } else {
            let open_sec = gio::Menu::new();
            open_sec.append(Some("Open"), Some("win.open"));
            if selected.len() == 1 && selected[0].is_dir {
                open_sec.append(Some("Open in New Tab"), Some("win.open-tab"));
                let bm_label = if self.sidebar.is_bookmarked(&selected[0].path) {
                    "Remove Bookmark"
                } else {
                    "Add Bookmark"
                };
                open_sec.append(Some(bm_label), Some("win.bookmark"));
            }
            if selected.len() == 1 && !selected[0].is_dir {
                open_sec.append(Some("Open With…"), Some("win.open-with"));
            }
            if selected.len() == 1 {
                open_sec.append(Some("Quick Look"), Some("win.quicklook"));
                if !selected[0].is_dir && ops::is_supported_archive(&selected[0].name) {
                    open_sec.append(Some("Extract Here"), Some("win.extract"));
                }
            }
            if selected.first().is_some_and(|e| e.is_dir) {
                open_sec.append(Some("Terminal in This Folder"), Some("win.dock-here"));
                open_sec.append(Some("Terminal in New Window"), Some("win.terminal"));
            }
            menu.append_section(None, &open_sec);

            let edit_sec = gio::Menu::new();
            edit_sec.append(Some("Cut"), Some("win.cut"));
            edit_sec.append(Some("Copy"), Some("win.copy"));
            edit_sec.append(Some("Duplicate"), Some("win.duplicate"));
            if selected.len() == 1 {
                edit_sec.append(Some("Rename"), Some("win.rename"));
            }
            edit_sec.append(Some("Compress…"), Some("win.compress"));
            edit_sec.append(Some("Copy Path"), Some("win.copy-path"));
            menu.append_section(None, &edit_sec);

            let del_sec = gio::Menu::new();
            del_sec.append(Some("Move to Trash"), Some("win.trash"));
            del_sec.append(Some("Delete Permanently…"), Some("win.delete-permanently"));
            if selected.len() == 1 {
                del_sec.append(Some("Properties"), Some("win.properties"));
            }
            menu.append_section(None, &del_sec);
        }

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&self.views.stack);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        self.track_and_popup(popover.upcast());
    }
}

fn next_copy_name(
    name: &str,
    is_dir: bool,
    force_suffix: bool,
    taken: &mut HashSet<String>,
) -> String {
    if !force_suffix && !taken.contains(name) {
        taken.insert(name.to_owned());
        return name.to_owned();
    }
    let (stem, ext) = if !is_dir {
        match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s, Some(e)),
            _ => (name, None),
        }
    } else {
        (name, None)
    };
    let candidate = |n: usize| -> String {
        let suffix = if n == 1 {
            " (copy)".to_owned()
        } else {
            format!(" (copy {n})")
        };
        match ext {
            Some(e) => format!("{stem}{suffix}.{e}"),
            None => format!("{stem}{suffix}"),
        }
    };
    let mut n = 1usize;
    loop {
        let cand = candidate(n);
        if !taken.contains(&cand) {
            taken.insert(cand.clone());
            return cand;
        }
        n += 1;
    }
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("flat");
    button
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_state_lifecycle() {
        let mut tab = TabState::new(1, Location::Local(PathBuf::from("/tmp")), Vec::new());
        assert!(!tab.load_cancel.load(Ordering::Relaxed));
        tab.cancel_async();
        assert!(tab.load_cancel.load(Ordering::Relaxed));
        assert!(tab.search_cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn next_copy_name_handles_files_dirs_and_collisions() {
        let mut taken = HashSet::from(["note.txt".to_owned(), "note (copy).txt".to_owned()]);
        assert_eq!(
            next_copy_name("note.txt", false, false, &mut taken),
            "note (copy 2).txt"
        );
        assert_eq!(
            next_copy_name("fresh.txt", false, true, &mut taken),
            "fresh (copy).txt"
        );
        assert_eq!(
            next_copy_name("dir.v1", true, true, &mut taken),
            "dir.v1 (copy)"
        );
        assert_eq!(
            next_copy_name(".env", false, true, &mut taken),
            ".env (copy)"
        );
    }
}
