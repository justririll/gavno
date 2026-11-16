use crate::window;
use adw::prelude::*;
const XML: &str = r#"<node><interface name="org.freedesktop.FileManager1">
<method name="ShowFolders"><arg type="as" direction="in" name="URIs"/><arg type="s" direction="in" name="StartupId"/></method>
<method name="ShowItems"><arg type="as" direction="in" name="URIs"/><arg type="s" direction="in" name="StartupId"/></method>
<method name="ShowItemProperties"><arg type="as" direction="in" name="URIs"/><arg type="s" direction="in" name="StartupId"/></method>
</interface></node>"#;
thread_local! {static CONNECTION:std::cell::RefCell<Option<gio::DBusConnection>>=const{std::cell::RefCell::new(None)};}
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
                        glib::Error::new(gio::IOErrorEnum::Failed, "Invalid FileManager1 interface")
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
                            match method {
                                "ShowFolders" => {
                                    for uri in uris {
                                        window::open(&app, gio::File::for_uri(&uri), Vec::new());
                                    }
                                }
                                "ShowItems" => window::show_items(&app, uris),
                                "ShowItemProperties" => {
                                    let state = window::open(
                                        &app,
                                        gio::File::for_path(glib::home_dir()),
                                        Vec::new(),
                                    );
                                    for uri in uris {
                                        let state = state.clone();
                                        glib::spawn_future_local(async move {
                                            let file = gio::File::for_uri(&uri);
                                            match crate::ops::query(&file, &gio::Cancellable::new())
                                                .await
                                            {
                                                Ok(info) => {
                                                    crate::model::attach(&info, &file);
                                                    let report = std::rc::Rc::new(glib::clone!(
                                                        #[weak]
                                                        state,
                                                        move |e: String| state.notify(&e)
                                                    ));
                                                    crate::preview::properties(
                                                        &state.window,
                                                        &info,
                                                        report,
                                                    );
                                                }
                                                Err(e) => state.notify(&e.to_string()),
                                            }
                                        });
                                    }
                                }
                                _ => {
                                    invocation.return_dbus_error(
                                        "org.freedesktop.DBus.Error.UnknownMethod",
                                        "Unknown method",
                                    );
                                    return;
                                }
                            }
                            invocation.return_value(Some(&().to_variant()));
                        }
                    ))
                    .build()?;
                gio::bus_own_name_on_connection(
                    &connection,
                    "org.freedesktop.FileManager1",
                    gio::BusNameOwnerFlags::NONE,
                    |_, _| {},
                    glib::clone!(
                        #[weak]
                        app,
                        move |_, _| window::report(
                            &app,
                            "FileManager1 is already owned by another file manager"
                        )
                    ),
                );
                CONNECTION.with(|slot| slot.replace(Some(connection)));
                Ok::<_, glib::Error>(())
            }
            .await;
            if let Err(e) = result {
                window::report(&app, format!("D-Bus: {e}"));
            }
        }
    ));
}
