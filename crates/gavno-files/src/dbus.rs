use crate::{
    icons::IconCache,
    model::{self, Location},
    preview, window,
};
use adw::prelude::*;
use std::{collections::HashMap, rc::Rc};

const XML: &str = r#"<node><interface name="org.freedesktop.FileManager1">
<method name="ShowFolders"><arg type="as" direction="in" name="URIs"/><arg type="s" direction="in" name="StartupId"/></method>
<method name="ShowItems"><arg type="as" direction="in" name="URIs"/><arg type="s" direction="in" name="StartupId"/></method>
<method name="ShowItemProperties"><arg type="as" direction="in" name="URIs"/><arg type="s" direction="in" name="StartupId"/></method>
</interface></node>"#;

thread_local! {
    static CONNECTION: std::cell::RefCell<Option<gio::DBusConnection>> = const { std::cell::RefCell::new(None) };
}

pub fn install(app: &adw::Application) {
    glib::spawn_future_local(glib::clone!(
        #[weak]
        app,
        async move {
            let result = async {
                let connection = gio::bus_get_future(gio::BusType::Session).await?;
                let node = gio::DBusNodeInfo::for_xml(XML)?;
                let interface = node
                    .lookup_interface("org.freedesktop.FileManager1")
                    .ok_or_else(|| {
                        glib::Error::new(
                            gio::IOErrorEnum::Failed,
                            "Invalid FileManager1 interface",
                        )
                    })?;
                connection
                    .register_object("/org/freedesktop/FileManager1", &interface)
                    .method_call(glib::clone!(
                        #[weak]
                        app,
                        move |_, _, _, _, method, parameters, invocation| {
                            let Some((uris, _startup)) = parameters.get::<(Vec<String>, String)>()
                            else {
                                invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.InvalidArgs",
                                    "Expected URIs and startup ID",
                                );
                                return;
                            };
                            if uris.iter().any(|uri| uri.contains('\0')) {
                                invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.InvalidArgs",
                                    "URI contains a null character",
                                );
                                return;
                            }
                            let res = match method {
                                "ShowFolders" => show_folders(&app, uris),
                                "ShowItems" => show_items(&app, uris),
                                "ShowItemProperties" => show_properties(&app, uris),
                                _ => {
                                    invocation.return_dbus_error(
                                        "org.freedesktop.DBus.Error.UnknownMethod",
                                        "Unknown method",
                                    );
                                    return;
                                }
                            };
                            match res {
                                Ok(()) => invocation.return_value(Some(&().to_variant())),
                                Err(msg) => invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.InvalidArgs",
                                    &msg,
                                ),
                            }
                        }
                    ))
                    .build()?;
                gio::bus_own_name_on_connection(
                    &connection,
                    "org.freedesktop.FileManager1",
                    gio::BusNameOwnerFlags::NONE,
                    |_, _| {},
                    |_, name| {
                        eprintln!("gavno-files: D-Bus name {name} is already owned");
                    },
                );
                CONNECTION.with(|slot| slot.replace(Some(connection)));
                Ok::<_, glib::Error>(())
            }
            .await;
            if let Err(e) = result {
                eprintln!("gavno-files D-Bus registration failed: {e}");
            }
        }
    ));
}

fn show_folders(app: &adw::Application, uris: Vec<String>) -> Result<(), String> {
    if uris.is_empty() {
        return Err("No folder URIs provided".to_owned());
    }
    let mut locs = Vec::with_capacity(uris.len());
    for uri in uris {
        locs.push(Location::from_uri(&uri)?);
    }
    for loc in locs {
        window::open(app, loc, Vec::new());
    }
    Ok(())
}

fn show_items(app: &adw::Application, uris: Vec<String>) -> Result<(), String> {
    if uris.is_empty() {
        return Err("No item URIs provided".to_owned());
    }
    let mut folders: HashMap<Location, Vec<String>> = HashMap::new();
    for uri in uris {
        let loc = Location::from_uri(&uri)?;
        if let Some(parent) = loc.parent() {
            folders.entry(parent).or_default().push(loc.to_uri());
        } else {
            folders.entry(loc).or_default();
        }
    }
    for (folder, selected) in folders {
        window::open(app, folder, selected);
    }
    Ok(())
}

fn show_properties(app: &adw::Application, uris: Vec<String>) -> Result<(), String> {
    if uris.is_empty() {
        return Err("No item URIs provided".to_owned());
    }
    let mut first_path = None;
    for uri in uris {
        match Location::from_uri(&uri)? {
            Location::Local(p) => {
                if first_path.is_none() {
                    first_path = Some(p);
                }
            }
            Location::Trash => {}
        }
    }
    let Some(path) = first_path else {
        return Err("Properties require a local file URI".to_owned());
    };
    let win = window::active_window(app).unwrap_or_else(|| {
        let initial = path
            .parent()
            .map(|p| Location::Local(p.to_path_buf()))
            .unwrap_or_else(|| Location::Local(glib::home_dir()));
        window::open(app, initial, Vec::new())
    });
    let icon_cache = IconCache::new();
    glib::spawn_future_local(async move {
        let p = path.clone();
        match gio::spawn_blocking(move || model::entry_from_path(&p, false)).await {
            Ok(Some(entry)) => {
                preview::show_properties(
                    &win.window,
                    entry,
                    &icon_cache,
                    false,
                    Rc::new(|_, _, _| {}),
                );
            }
            _ => win.notify(&format!(
                "Cannot read properties for {}",
                path.display()
            )),
        }
    });
    Ok(())
}
