use crate::views::Report;
use adw::prelude::*;
use std::{cell::RefCell, rc::Rc};
type Navigate = Rc<dyn Fn(gio::File)>;
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

pub fn build(
    parent: &adw::ApplicationWindow,
    navigate: Navigate,
    report: Report,
) -> (gtk::ScrolledWindow, gio::VolumeMonitor) {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 2);
    container.add_css_class("places");
    container.set_width_request(196);
    let monitor = gio::VolumeMonitor::get();
    let places = Rc::new(RefCell::new(
        USER_PLACES
            .iter()
            .map(|(label, icon, _)| (*label, *icon, glib::home_dir().join(label)))
            .collect::<Vec<_>>(),
    ));
    let bookmarks = Rc::new(RefCell::new(Vec::<(String, gio::File)>::new()));
    let refresh: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[weak]
        container,
        #[weak]
        parent,
        #[weak]
        monitor,
        #[strong]
        places,
        #[strong]
        navigate,
        #[strong]
        report,
        #[strong]
        bookmarks,
        move || {
            while let Some(child) = container.first_child() {
                container.remove(&child);
            }
            heading(&container, "Places");
            place(
                &container,
                "Home",
                "user-home-symbolic",
                gio::File::for_path(glib::home_dir()),
                &navigate,
            );
            for (label, icon, path) in places.borrow().iter() {
                place(
                    &container,
                    label,
                    icon,
                    gio::File::for_path(path),
                    &navigate,
                );
            }
            place(
                &container,
                "Trash",
                "user-trash-symbolic",
                gio::File::for_uri("trash:///"),
                &navigate,
            );
            if !bookmarks.borrow().is_empty() {
                heading(&container, "Bookmarks");
            }
            for (label, file) in bookmarks.borrow().iter() {
                place(
                    &container,
                    label,
                    "folder-symbolic",
                    file.clone(),
                    &navigate,
                );
            }
            heading(&container, "Devices");
            place(
                &container,
                "System",
                "drive-harddisk-symbolic",
                gio::File::for_path("/"),
                &navigate,
            );
            for mount in monitor.mounts() {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
                let button = place(
                    &row,
                    &mount.name(),
                    "drive-removable-media-symbolic",
                    mount.root(),
                    &navigate,
                );
                button.set_hexpand(true);
                if mount.can_unmount() || mount.can_eject() {
                    let eject = gtk::Button::from_icon_name("media-eject-symbolic");
                    eject.add_css_class("flat");
                    eject.set_tooltip_text(Some("Eject drive"));
                    eject.connect_clicked(glib::clone!(
                        #[weak]
                        parent,
                        #[strong]
                        mount,
                        #[strong]
                        report,
                        move |_| {
                            let operation = gtk::MountOperation::new(Some(&parent));
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
                    ));
                    row.append(&eject);
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
                                    Ok(()) =>
                                        if let Some(mount) = volume.get_mount() {
                                            navigate(mount.root());
                                        },
                                    Err(e) => report(e.to_string()),
                                }
                            ),
                        );
                    }
                ));
                container.append(&button);
            }
        }
    ));
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
        #[weak]
        monitor,
        move |_| {
            for handler in handlers.borrow_mut().drain(..) {
                monitor.disconnect(handler);
            }
        }
    ));
    glib::spawn_future_local(glib::clone!(
        #[strong]
        refresh,
        #[strong]
        report,
        async move {
            match gio::spawn_blocking(|| {
                USER_PLACES
                    .iter()
                    .map(|(label, icon, dir)| {
                        (
                            *label,
                            *icon,
                            glib::user_special_dir(*dir)
                                .unwrap_or_else(|| glib::home_dir().join(label)),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .await
            {
                Ok(loaded) => {
                    places.replace(loaded);
                    refresh();
                }
                Err(_) => report("User directory worker failed".into()),
            }
        }
    ));
    let bookmark_file = gio::File::for_path(glib::user_config_dir().join("gtk-3.0/bookmarks"));
    glib::spawn_future_local(async move {
        match bookmark_file.load_contents_future().await {
            Ok((bytes, _)) => {
                for line in String::from_utf8_lossy(&bytes).lines() {
                    if line.contains('\0') {
                        report("Invalid bookmark containing a null character".into());
                        continue;
                    }
                    let (uri, label) = line.split_once(' ').unwrap_or((line, ""));
                    if uri.is_empty() || uri.contains('\0') {
                        continue;
                    }
                    let file = gio::File::for_uri(uri);
                    let label = if label.is_empty() {
                        file.basename()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|| uri.into())
                    } else {
                        label.into()
                    };
                    bookmarks.borrow_mut().push((label, file));
                }
                refresh();
            }
            Err(e) if e.matches(gio::IOErrorEnum::NotFound) => {}
            Err(e) => report(e.to_string()),
        }
    });
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&container)
        .build();
    (scroll, monitor)
}
fn heading(container: &gtk::Box, text: &str) {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.);
    label.add_css_class("place-heading");
    container.append(&label);
}
fn place(
    container: &gtk::Box,
    label: &str,
    icon: &str,
    file: gio::File,
    navigate: &Navigate,
) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.set_tooltip_text(Some(&file.parse_name()));
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.append(&gtk::Image::from_icon_name(icon));
    let label = gtk::Label::new(Some(label));
    label.set_xalign(0.);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&label);
    button.set_child(Some(&row));
    button.connect_clicked(glib::clone!(
        #[strong]
        navigate,
        move |_| navigate(file.clone())
    ));
    container.append(&button);
    button
}
