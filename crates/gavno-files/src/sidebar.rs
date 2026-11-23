use crate::{
    clipboard,
    model::{self, Location},
    views::Report,
};
use adw::prelude::*;
use gtk::gdk;
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_BM_SEQ: AtomicU64 = AtomicU64::new(1);

pub type NavigateFn = Rc<dyn Fn(Location)>;
pub type NewTabFn = Rc<dyn Fn(Location)>;
pub type OpenTermFn = Rc<dyn Fn(PathBuf, bool)>;
pub type SidebarDropFn = Rc<dyn Fn(Location, Vec<String>, bool, gdk::Drop)>;

const USER_PLACES: [(&str, &str, glib::UserDirectory); 6] = [
    (
        "Desktop",
        "user-desktop-symbolic",
        glib::UserDirectory::Desktop,
    ),
    (
        "Documents",
        "folder-documents-symbolic",
        glib::UserDirectory::Documents,
    ),
    (
        "Downloads",
        "folder-download-symbolic",
        glib::UserDirectory::Downloads,
    ),
    ("Music", "folder-music-symbolic", glib::UserDirectory::Music),
    (
        "Pictures",
        "folder-pictures-symbolic",
        glib::UserDirectory::Pictures,
    ),
    (
        "Videos",
        "folder-videos-symbolic",
        glib::UserDirectory::Videos,
    ),
];

#[derive(Clone)]
pub struct SidebarCallbacks {
    pub navigate: NavigateFn,
    pub new_tab: NewTabFn,
    pub open_term: OpenTermFn,
    pub on_drop: SidebarDropFn,
    pub report: Report,
}

#[derive(Clone)]
struct ProcMount {
    label: String,
    icon: String,
    path: PathBuf,
}

#[derive(Clone)]
struct DeviceEntry {
    label: String,
    icon: String,
    path: PathBuf,
    mount: Option<gio::Mount>,
}

#[derive(Clone)]
pub struct Sidebar {
    pub scroll: gtk::ScrolledWindow,
    buttons: Rc<RefCell<Vec<(Location, gtk::Button)>>>,
    trash_badge: Rc<RefCell<Option<gtk::Label>>>,
    trash_count_val: Rc<Cell<usize>>,
    bookmarks: Rc<RefCell<Vec<(String, PathBuf, String)>>>,
    bookmark_rev: Rc<Cell<u64>>,
    active_location: Rc<RefCell<Location>>,
    refresh: Rc<dyn Fn()>,
    #[allow(dead_code)]
    bookmark_monitor: Rc<RefCell<Option<gio::FileMonitor>>>,
}

impl Sidebar {
    pub fn build(parent: &adw::ApplicationWindow, callbacks: SidebarCallbacks) -> Self {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 2);
        container.add_css_class("places");
        container.set_width_request(196);

        let monitor = gio::VolumeMonitor::get();
        let places = Rc::new(RefCell::new(load_user_places()));
        let bookmarks: Rc<RefCell<Vec<(String, PathBuf, String)>>> =
            Rc::new(RefCell::new(read_bookmarks_sync()));
        let bookmark_rev = Rc::new(Cell::new(0u64));
        let proc_mounts: Rc<RefCell<Vec<DeviceEntry>>> = Rc::new(RefCell::new(Vec::new()));
        let buttons: Rc<RefCell<Vec<(Location, gtk::Button)>>> = Rc::new(RefCell::new(Vec::new()));
        let trash_badge: Rc<RefCell<Option<gtk::Label>>> = Rc::new(RefCell::new(None));
        let trash_count_val = Rc::new(Cell::new(crate::trash::trash_count()));
        let active_location = Rc::new(RefCell::new(Location::Local(glib::home_dir())));
        let bookmark_monitor: Rc<RefCell<Option<gio::FileMonitor>>> = Rc::new(RefCell::new(None));
        let refresh_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

        let on_remove_bookmark: Rc<dyn Fn(String)> = Rc::new(glib::clone!(
            #[strong]
            bookmarks,
            #[strong]
            bookmark_rev,
            #[strong]
            refresh_slot,
            #[strong(rename_to = report)]
            callbacks.report,
            move |uri: String| {
                bookmark_rev.set(bookmark_rev.get().wrapping_add(1));
                bookmarks.borrow_mut().retain(|(_, _, u)| u != &uri);
                let snapshot: Vec<(String, String)> = bookmarks
                    .borrow()
                    .iter()
                    .map(|(l, _, u)| (u.clone(), l.clone()))
                    .collect();
                if let Some(ref r) = *refresh_slot.borrow() {
                    r();
                }
                let report = report.clone();
                glib::spawn_future_local(async move {
                    if let Ok(Err(e)) =
                        gio::spawn_blocking(move || write_bookmarks_sync(&snapshot)).await
                    {
                        report(e);
                    }
                });
            }
        ));

        let refresh: Rc<dyn Fn()> = Rc::new(glib::clone!(
            #[weak]
            container,
            #[weak]
            parent,
            #[strong]
            monitor,
            #[strong]
            places,
            #[strong]
            bookmarks,
            #[strong]
            proc_mounts,
            #[strong]
            buttons,
            #[strong]
            trash_badge,
            #[strong]
            trash_count_val,
            #[strong]
            active_location,
            #[strong]
            callbacks,
            #[strong]
            on_remove_bookmark,
            move || {
                while let Some(child) = container.first_child() {
                    container.remove(&child);
                }
                buttons.borrow_mut().clear();
                trash_badge.replace(None);

                heading(&container, "Places");
                let home = glib::home_dir();
                add_place(
                    &container,
                    &buttons,
                    "Home",
                    "user-home-symbolic",
                    Location::Local(home.clone()),
                    None,
                    &callbacks,
                    None,
                    &on_remove_bookmark,
                );
                for (label, icon, path) in places.borrow().iter() {
                    if path == &home || !path.exists() {
                        continue;
                    }
                    add_place(
                        &container,
                        &buttons,
                        label,
                        icon,
                        Location::Local(path.clone()),
                        None,
                        &callbacks,
                        None,
                        &on_remove_bookmark,
                    );
                }
                let projects = home.join("Projects");
                if projects.is_dir() {
                    add_place(
                        &container,
                        &buttons,
                        "Projects",
                        "code-context-symbolic",
                        Location::Local(projects),
                        None,
                        &callbacks,
                        None,
                        &on_remove_bookmark,
                    );
                }
                let trash_btn = add_place(
                    &container,
                    &buttons,
                    "Trash",
                    "user-trash-symbolic",
                    Location::Trash,
                    None,
                    &callbacks,
                    None,
                    &on_remove_bookmark,
                );
                if let Some(row) = trash_btn.child().and_downcast::<gtk::Box>() {
                    let count = trash_count_val.get();
                    let badge = gtk::Label::new(Some(&count.to_string()));
                    badge.add_css_class("capacity");
                    badge.set_hexpand(true);
                    badge.set_xalign(1.);
                    badge.set_visible(count > 0);
                    row.append(&badge);
                    trash_badge.replace(Some(badge));
                }

                if !bookmarks.borrow().is_empty() {
                    heading(&container, "Bookmarks");
                }
                for (label, path, raw_uri) in bookmarks.borrow().iter() {
                    add_place(
                        &container,
                        &buttons,
                        label,
                        "folder-symbolic",
                        Location::Local(path.clone()),
                        Some(raw_uri.clone()),
                        &callbacks,
                        None,
                        &on_remove_bookmark,
                    );
                }

                heading(&container, "Devices");
                let system_btn = add_place(
                    &container,
                    &buttons,
                    "System",
                    "drive-harddisk-symbolic",
                    Location::Local(PathBuf::from("/")),
                    None,
                    &callbacks,
                    None,
                    &on_remove_bookmark,
                );
                capacity(&system_btn, PathBuf::from("/"));

                let mut seen_paths = HashSet::new();
                seen_paths.insert(PathBuf::from("/"));

                let mut device_list = Vec::new();
                for mount in monitor.mounts() {
                    if let Some(path) = mount.root().path() {
                        if seen_paths.insert(path.clone()) {
                            device_list.push(DeviceEntry {
                                label: mount.name().to_string(),
                                icon: "drive-removable-media-symbolic".to_owned(),
                                path,
                                mount: Some(mount),
                            });
                        }
                    }
                }
                for dev in proc_mounts.borrow().iter() {
                    if seen_paths.insert(dev.path.clone()) {
                        device_list.push(dev.clone());
                    }
                }

                for dev in device_list {
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
                    let mount_opt = dev.mount.clone();
                    let button = add_place(
                        &row,
                        &buttons,
                        &dev.label,
                        &dev.icon,
                        Location::Local(dev.path.clone()),
                        None,
                        &callbacks,
                        mount_opt.clone(),
                        &on_remove_bookmark,
                    );
                    button.set_hexpand(true);
                    capacity(&button, dev.path.clone());
                    if let Some(mount) = mount_opt {
                        if mount.can_unmount() || mount.can_eject() {
                            let eject = gtk::Button::from_icon_name("media-eject-symbolic");
                            eject.add_css_class("flat");
                            eject.set_tooltip_text(Some("Eject drive"));
                            let report = callbacks.report.clone();
                            eject.connect_clicked(glib::clone!(
                                #[weak]
                                parent,
                                #[strong]
                                mount,
                                #[strong]
                                report,
                                move |_| eject_mount(&parent, &mount, &report)
                            ));
                            row.append(&eject);
                        }
                    }
                    container.append(&row);
                }

                for volume in monitor
                    .volumes()
                    .into_iter()
                    .filter(|v| v.get_mount().is_none())
                {
                    let button = gtk::Button::with_label(&volume.name());
                    button.add_css_class("flat");
                    button.add_css_class("place");
                    let navigate = callbacks.navigate.clone();
                    let report = callbacks.report.clone();
                    button.connect_clicked(glib::clone!(
                        #[weak]
                        parent,
                        #[strong]
                        navigate,
                        #[strong]
                        report,
                        move |_| {
                            let op = gtk::MountOperation::new(Some(&parent));
                            volume.mount(
                                gio::MountMountFlags::NONE,
                                Some(&op),
                                gio::Cancellable::NONE,
                                glib::clone!(
                                    #[strong]
                                    navigate,
                                    #[strong]
                                    report,
                                    #[strong]
                                    volume,
                                    move |r| match r {
                                        Ok(()) => {
                                            if let Some(mount) = volume.get_mount() {
                                                if let Some(p) = mount.root().path() {
                                                    navigate(Location::Local(p));
                                                }
                                            }
                                        }
                                        Err(e) => report(e.to_string()),
                                    }
                                ),
                            );
                        }
                    ));
                    container.append(&button);
                }

                apply_active(&buttons.borrow(), &active_location.borrow());
            }
        ));

        refresh_slot.replace(Some(refresh.clone()));
        refresh();

        let mut handlers = Vec::new();
        for signal in [
            "mount-added",
            "mount-removed",
            "mount-changed",
            "volume-added",
            "volume-removed",
            "volume-changed",
        ] {
            let refresh = refresh.clone();
            handlers.push(monitor.connect_local(signal, false, move |_| {
                refresh();
                None
            }));
        }
        let handlers = RefCell::new(handlers);
        container.connect_destroy(glib::clone!(
            #[strong]
            monitor,
            move |_| {
                for handler in handlers.borrow_mut().drain(..) {
                    monitor.disconnect(handler);
                }
            }
        ));

        // Load /proc/mounts asynchronously so extra disks/NTFS/USB mounts show up even without udisks2.
        glib::spawn_future_local(glib::clone!(
            #[strong]
            proc_mounts,
            #[strong]
            refresh,
            async move {
                if let Ok(list) = gio::spawn_blocking(read_proc_mounts_sync).await {
                    let entries = list
                        .into_iter()
                        .map(|m| DeviceEntry {
                            label: m.label,
                            icon: m.icon,
                            path: m.path,
                            mount: None,
                        })
                        .collect();
                    proc_mounts.replace(entries);
                    refresh();
                }
            }
        ));

        // Watch ~/.config/gtk-3.0/bookmarks for live updates.
        let bpath = bookmark_file_path();
        if let Ok(bm) = gio::File::for_path(&bpath).monitor_file(
            gio::FileMonitorFlags::WATCH_MOVES,
            gio::Cancellable::NONE,
        ) {
            bm.connect_changed(glib::clone!(
                #[strong]
                bookmarks,
                #[strong]
                bookmark_rev,
                #[strong]
                refresh,
                move |_, _, _, _| {
                    let rev = bookmark_rev.get();
                    glib::spawn_future_local(glib::clone!(
                        #[strong]
                        bookmarks,
                        #[strong]
                        bookmark_rev,
                        #[strong]
                        refresh,
                        async move {
                            if let Ok(loaded) = gio::spawn_blocking(read_bookmarks_sync).await {
                                if bookmark_rev.get() == rev {
                                    bookmarks.replace(loaded);
                                    refresh();
                                }
                            }
                        }
                    ));
                }
            ));
            bookmark_monitor.replace(Some(bm));
        }

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&container)
            .build();

        Self {
            scroll,
            buttons,
            trash_badge,
            trash_count_val,
            bookmarks,
            bookmark_rev,
            active_location,
            refresh,
            bookmark_monitor,
        }
    }

    pub fn set_active(&self, loc: &Location) {
        self.active_location.replace(loc.clone());
        apply_active(&self.buttons.borrow(), loc);
    }

    pub fn set_trash_count(&self, count: usize) {
        self.trash_count_val.set(count);
        if let Some(label) = self.trash_badge.borrow().as_ref() {
            label.set_text(&count.to_string());
            label.set_visible(count > 0);
        }
    }

    pub fn is_bookmarked(&self, path: &Path) -> bool {
        self.bookmarks.borrow().iter().any(|(_, p, _)| p == path)
    }

    pub fn add_bookmark(&self, path: &Path) {
        let uri = model::file_uri_from_path(path);
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        {
            let mut bm = self.bookmarks.borrow_mut();
            if bm.iter().any(|(_, p, _)| p == path) {
                return;
            }
            bm.push((label, path.to_path_buf(), uri));
        }
        self.bookmark_rev.set(self.bookmark_rev.get().wrapping_add(1));
        let snapshot: Vec<(String, String)> = self
            .bookmarks
            .borrow()
            .iter()
            .map(|(l, _, u)| (u.clone(), l.clone()))
            .collect();
        (self.refresh)();
        glib::spawn_future_local(async move {
            let _ = gio::spawn_blocking(move || write_bookmarks_sync(&snapshot)).await;
        });
    }

    pub fn remove_bookmark(&self, uri: &str) {
        self.bookmark_rev.set(self.bookmark_rev.get().wrapping_add(1));
        self.bookmarks.borrow_mut().retain(|(_, _, u)| u != uri);
        let snapshot: Vec<(String, String)> = self
            .bookmarks
            .borrow()
            .iter()
            .map(|(l, _, u)| (u.clone(), l.clone()))
            .collect();
        (self.refresh)();
        glib::spawn_future_local(async move {
            let _ = gio::spawn_blocking(move || write_bookmarks_sync(&snapshot)).await;
        });
    }
}

fn apply_active(buttons: &[(Location, gtk::Button)], current: &Location) {
    for (loc, btn) in buttons {
        if loc == current {
            btn.add_css_class("on");
        } else {
            btn.remove_css_class("on");
        }
    }
}

fn load_user_places() -> Vec<(&'static str, &'static str, PathBuf)> {
    USER_PLACES
        .iter()
        .map(|(label, icon, dir)| {
            (
                *label,
                *icon,
                glib::user_special_dir(*dir).unwrap_or_else(|| glib::home_dir().join(label)),
            )
        })
        .collect()
}

fn heading(container: &gtk::Box, text: &str) {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.);
    label.add_css_class("place-heading");
    container.append(&label);
}

#[allow(clippy::too_many_arguments)]
fn add_place(
    container: &gtk::Box,
    buttons: &Rc<RefCell<Vec<(Location, gtk::Button)>>>,
    label: &str,
    icon: &str,
    location: Location,
    bookmark_uri: Option<String>,
    callbacks: &SidebarCallbacks,
    mount: Option<gio::Mount>,
    on_remove_bookmark: &Rc<dyn Fn(String)>,
) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("place");
    button.set_tooltip_text(Some(&location.parse_name()));
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.append(&gtk::Image::from_icon_name(icon));
    let lbl = gtk::Label::new(Some(label));
    lbl.set_xalign(0.);
    lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&lbl);
    button.set_child(Some(&row));

    let nav = callbacks.navigate.clone();
    button.connect_clicked(glib::clone!(
        #[strong]
        location,
        #[strong]
        nav,
        move |_| nav(location.clone())
    ));

    // Middle-click opens in a new tab.
    let mid = gtk::GestureClick::new();
    mid.set_button(2);
    let new_tab = callbacks.new_tab.clone();
    mid.connect_pressed(glib::clone!(
        #[strong]
        location,
        #[strong]
        new_tab,
        move |gesture, _, _, _| {
            new_tab(location.clone());
            gesture.set_state(gtk::EventSequenceState::Claimed);
        }
    ));
    button.add_controller(mid);

    // Right-click context menu.
    let rclick = gtk::GestureClick::new();
    rclick.set_button(3);
    rclick.connect_pressed(glib::clone!(
        #[weak]
        button,
        #[strong]
        location,
        #[strong]
        bookmark_uri,
        #[strong]
        mount,
        #[strong]
        callbacks,
        #[strong]
        on_remove_bookmark,
        move |gesture, _, x, y| {
            show_place_menu(
                &button,
                x,
                y,
                &location,
                bookmark_uri.as_deref(),
                mount.as_ref(),
                &callbacks,
                &on_remove_bookmark,
            );
            gesture.set_state(gtk::EventSequenceState::Claimed);
        }
    ));
    button.add_controller(rclick);

    // Drag-and-drop onto sidebar entry (copies/moves into folder or Trash).
    let drop_target = gtk::DropTargetAsync::new(
        Some(gdk::ContentFormats::new(&[clipboard::URI_LIST])),
        gdk::DragAction::COPY | gdk::DragAction::MOVE,
    );
    drop_target.connect_drag_enter(glib::clone!(
        #[weak]
        button,
        #[upgrade_or]
        gdk::DragAction::empty(),
        move |controller, drop, _, _| {
            button.add_css_class("drop-target");
            crate::views::preferred_drop_action(controller.current_event_state(), drop)
        }
    ));
    drop_target.connect_drag_motion(move |controller, drop, _, _| {
        crate::views::preferred_drop_action(controller.current_event_state(), drop)
    });
    drop_target.connect_drag_leave(glib::clone!(
        #[weak]
        button,
        move |_, _| button.remove_css_class("drop-target")
    ));
    let on_drop = callbacks.on_drop.clone();
    let report = callbacks.report.clone();
    drop_target.connect_drop(glib::clone!(
        #[weak]
        button,
        #[strong]
        location,
        #[strong]
        on_drop,
        #[strong]
        report,
        #[upgrade_or]
        false,
        move |controller, drop, _, _| {
            button.remove_css_class("drop-target");
            let moving = crate::views::is_move_drop(controller.current_event_state(), drop);
            let drop = drop.clone();
            let location = location.clone();
            let on_drop = on_drop.clone();
            let report = report.clone();
            glib::spawn_future_local(async move {
                match clipboard::read_drop(&drop).await {
                    Ok(uris) => on_drop(location, uris, moving, drop),
                    Err(e) => {
                        report(e);
                        drop.finish(gdk::DragAction::empty());
                    }
                }
            });
            true
        }
    ));
    button.add_controller(drop_target);

    buttons.borrow_mut().push((location, button.clone()));
    container.append(&button);
    button
}

#[allow(clippy::too_many_arguments)]
fn show_place_menu(
    anchor: &gtk::Button,
    x: f64,
    y: f64,
    location: &Location,
    bookmark_uri: Option<&str>,
    mount: Option<&gio::Mount>,
    callbacks: &SidebarCallbacks,
    on_remove_bookmark: &Rc<dyn Fn(String)>,
) {
    let popover = gtk::Popover::new();
    popover.set_parent(anchor);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.connect_closed(|p| p.unparent());

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
    vbox.set_width_request(210);

    let add_btn = |text: &str, cb: Box<dyn FnOnce()>| {
        let btn = gtk::Button::with_label(text);
        btn.add_css_class("flat");
        if let Some(lbl) = btn.child().and_downcast::<gtk::Label>() {
            lbl.set_xalign(0.);
        }
        let cb = RefCell::new(Some(cb));
        btn.connect_clicked(glib::clone!(
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                if let Some(f) = cb.borrow_mut().take() {
                    f();
                }
            }
        ));
        vbox.append(&btn);
    };

    let loc_open = location.clone();
    let nav = callbacks.navigate.clone();
    add_btn("Open", Box::new(move || nav(loc_open)));

    let loc_tab = location.clone();
    let new_tab = callbacks.new_tab.clone();
    add_btn("Open in New Tab", Box::new(move || new_tab(loc_tab)));

    if let Some(path) = location.local_path() {
        let p1 = path.to_path_buf();
        let t1 = callbacks.open_term.clone();
        add_btn("Terminal Here (F4)", Box::new(move || t1(p1, false)));

        let p2 = path.to_path_buf();
        let t2 = callbacks.open_term.clone();
        add_btn(
            "Terminal in New Window",
            Box::new(move || t2(p2, true)),
        );
    }

    if let Some(uri) = bookmark_uri {
        let uri_owned = uri.to_owned();
        let remove = on_remove_bookmark.clone();
        add_btn(
            "Remove Bookmark",
            Box::new(move || remove(uri_owned)),
        );
    }

    if let Some(mount) = mount.cloned() {
        if mount.can_unmount() || mount.can_eject() {
            let report = callbacks.report.clone();
            let parent_win = anchor.root().and_downcast::<adw::ApplicationWindow>();
            add_btn(
                if mount.can_eject() { "Eject" } else { "Unmount" },
                Box::new(move || {
                    if let Some(win) = parent_win {
                        eject_mount(&win, &mount, &report);
                    }
                }),
            );
        }
    }

    popover.set_child(Some(&vbox));
    popover.popup();
}

fn eject_mount(parent: &adw::ApplicationWindow, mount: &gio::Mount, report: &Report) {
    let operation = gtk::MountOperation::new(Some(parent));
    let report = report.clone();
    if mount.can_eject() {
        mount.eject_with_operation(
            gio::MountUnmountFlags::NONE,
            Some(&operation),
            gio::Cancellable::NONE,
            move |r| {
                if let Err(e) = r {
                    report(e.to_string());
                }
            },
        );
    } else {
        mount.unmount_with_operation(
            gio::MountUnmountFlags::NONE,
            Some(&operation),
            gio::Cancellable::NONE,
            move |r| {
                if let Err(e) = r {
                    report(e.to_string());
                }
            },
        );
    }
}

fn capacity(button: &gtk::Button, path: PathBuf) {
    glib::spawn_future_local(glib::clone!(
        #[weak]
        button,
        async move {
            let Ok(Some(bytes)) = gio::spawn_blocking(move || statvfs_total(&path)).await else {
                return;
            };
            if bytes == 0 {
                return;
            }
            if let Some(row) = button.child().and_downcast::<gtk::Box>() {
                let label = gtk::Label::new(Some(&model::format_capacity(bytes)));
                label.add_css_class("capacity");
                label.set_hexpand(true);
                label.set_xalign(1.);
                row.append(&label);
            }
        }
    ));
}

fn statvfs_total(path: &Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    let frsize = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    Some((stat.f_blocks as u64).saturating_mul(frsize))
}

fn bookmark_file_path() -> PathBuf {
    glib::user_config_dir().join("gtk-3.0/bookmarks")
}

fn read_bookmarks_sync() -> Vec<(String, PathBuf, String)> {
    let Ok(text) = std::fs::read_to_string(bookmark_file_path()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.contains('\0') {
            continue;
        }
        let (uri, label) = line.split_once(' ').unwrap_or((line, ""));
        if uri.is_empty() {
            continue;
        }
        let Ok(Location::Local(path)) = Location::from_uri(uri) else {
            continue;
        };
        let label = if label.trim().is_empty() {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned())
        } else {
            label.trim().to_owned()
        };
        out.push((label, path, uri.to_owned()));
    }
    out
}

fn write_bookmarks_sync(items: &[(String, String)]) -> Result<(), String> {
    static BM_WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = BM_WRITER.lock().map_err(|_| "Bookmark writer failed")?;
    let path = bookmark_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut text = String::new();
    for (uri, label) in items {
        let clean_uri: String = uri.chars().filter(|&c| c != '\n' && c != '\r').collect();
        let clean_label: String = label.chars().filter(|&c| c != '\n' && c != '\r').collect();
        if clean_label.is_empty() {
            text.push_str(&clean_uri);
        } else {
            text.push_str(&clean_uri);
            text.push(' ');
            text.push_str(&clean_label);
        }
        text.push('\n');
    }
    let seq = NEXT_BM_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{seq}", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, text) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

fn unescape_mount_field(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let (Some(a), Some(b), Some(c)) = (
                (bytes[i + 1] as char).to_digit(8),
                (bytes[i + 2] as char).to_digit(8),
                (bytes[i + 3] as char).to_digit(8),
            ) {
                let val = ((a << 6) | (b << 3) | c) as u8;
                out.push(val);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_proc_mounts_sync() -> Vec<ProcMount> {
    let Ok(text) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(_src) = parts.next() else { continue };
        let Some(raw_mp) = parts.next() else { continue };
        let Some(fstype) = parts.next() else { continue };
        if matches!(
            fstype,
            "proc"
                | "sysfs"
                | "tmpfs"
                | "devtmpfs"
                | "devpts"
                | "cgroup"
                | "cgroup2"
                | "securityfs"
                | "debugfs"
                | "tracefs"
                | "pstore"
                | "efivarfs"
                | "autofs"
                | "mqueue"
                | "hugetlbfs"
                | "bpf"
                | "configfs"
                | "fusectl"
                | "fuse.portal"
                | "overlay"
                | "ramfs"
                | "nsfs"
                | "binfmt_misc"
                | "rpc_pipefs"
        ) {
            continue;
        }
        let mp = unescape_mount_field(raw_mp);
        if mp == "/"
            || mp == "/home"
            || mp.starts_with("/boot")
            || mp.starts_with("/efi")
            || mp.starts_with("/sys")
            || mp.starts_with("/proc")
            || mp.starts_with("/dev")
            || (mp.starts_with("/run/") && !mp.starts_with("/run/media/"))
            || mp.starts_with("/var/")
            || mp.starts_with("/usr/")
        {
            continue;
        }
        let path = PathBuf::from(&mp);
        if !seen.insert(path.clone()) || !path.is_dir() {
            continue;
        }
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| mp.clone());
        let icon = if mp.starts_with("/run/media/") || mp.starts_with("/media/") {
            "drive-removable-media-symbolic"
        } else {
            "drive-harddisk-symbolic"
        };
        out.push(ProcMount {
            label,
            icon: icon.to_owned(),
            path,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_octal_and_utf8_mount_fields() {
        assert_eq!(
            unescape_mount_field("/run/media/kirill/My\\040Drive_Флешка"),
            "/run/media/kirill/My Drive_Флешка"
        );
    }
}
