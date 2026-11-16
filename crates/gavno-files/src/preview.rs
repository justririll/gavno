use crate::{model, ops, thumbs::Thumbnails, views::Report};
use adw::prelude::*;

pub fn show(
    parent: &adw::ApplicationWindow,
    info: &gio::FileInfo,
    thumbs: Thumbnails,
    report: Report,
) {
    let window = gtk::Window::builder()
        .title(info.display_name())
        .transient_for(parent)
        .modal(true)
        .default_width(660)
        .default_height(460)
        .build();
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new(&info.display_name(), &model::file(info).parse_name());
    header.set_title_widget(Some(&title));
    layout.append(&header);
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .build();
    let metadata = gtk::Label::new(Some(&format!(
        "{} · {} · {}\n{}",
        model::kind(info),
        model::format_size(info.size().max(0) as u64),
        model::modified(info),
        model::permissions(info)
    )));
    metadata.set_wrap(true);
    metadata.set_margin_top(12);
    metadata.set_margin_bottom(12);
    layout.append(&scroll);
    layout.append(&metadata);
    window.set_child(Some(&layout));
    let cancel = gio::Cancellable::new();
    window.connect_destroy(glib::clone!(
        #[strong]
        cancel,
        move |_| cancel.cancel()
    ));
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        window,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key == gtk::gdk::Key::space || key == gtk::gdk::Key::Escape {
                window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    ));
    window.add_controller(keys);
    window.present();
    let info = info.clone();
    glib::spawn_future_local(glib::clone!(
        #[weak]
        scroll,
        async move {
            if crate::thumbs::supported(&info) {
                match thumbs.get(&info).await {
                    Ok(Some(texture)) => {
                        let picture = gtk::Picture::for_paintable(&texture);
                        picture.set_can_shrink(true);
                        scroll.set_child(Some(&picture));
                    }
                    Ok(None) => {}
                    Err(e) => report(e),
                }
            } else if info.content_type().is_some_and(|t| {
                gio::content_type_is_a(&t, "text/plain")
                    || t == "application/json"
                    || t == "application/xml"
            }) && !model::is_dir(&info)
            {
                let result = read_text(&model::file(&info), &cancel).await;
                match result {
                    Ok(text) => {
                        let view = gtk::TextView::new();
                        view.set_editable(false);
                        view.set_cursor_visible(false);
                        view.set_monospace(true);
                        view.set_left_margin(16);
                        view.set_right_margin(16);
                        view.set_top_margin(12);
                        view.buffer().set_text(&text);
                        scroll.set_child(Some(&view));
                    }
                    Err(e) => {
                        if !ops::is_cancelled(&e) {
                            report(e.to_string());
                        }
                    }
                }
            } else {
                let status = adw::StatusPage::builder()
                    .icon_name(if model::is_dir(&info) {
                        "folder-symbolic"
                    } else {
                        "text-x-generic-symbolic"
                    })
                    .title("No preview available")
                    .description("File details are shown below.")
                    .build();
                scroll.set_child(Some(&status));
            }
        }
    ));
}
async fn read_text(file: &gio::File, cancel: &gio::Cancellable) -> Result<String, glib::Error> {
    let (send, receive) = futures_channel::oneshot::channel();
    file.read_async(glib::Priority::DEFAULT, Some(cancel), move |r| {
        let _ = send.send(r);
    });
    let stream = receive.await.unwrap_or_else(|_| {
        Err(glib::Error::new(
            gio::IOErrorEnum::Cancelled,
            "Preview cancelled",
        ))
    })?;
    let mut bytes = Vec::new();
    while bytes.len() < 65536 {
        cancel.set_error_if_cancelled()?;
        let chunk = stream
            .read_bytes_future(8192.min(65536 - bytes.len()), glib::Priority::DEFAULT)
            .await?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    stream.close_future(glib::Priority::DEFAULT).await?;
    let mut text = String::from_utf8_lossy(&bytes).replace('\0', "�");
    if bytes.len() == 65536 {
        text.push_str("\n\n[Preview limited to 64 KiB]");
    }
    Ok(text)
}

pub fn properties(parent: &adw::ApplicationWindow, info: &gio::FileInfo, report: Report) {
    let body = format!(
        "Type: {}\nModified: {}\nPermissions: {}\nLocation: {}",
        model::kind(info),
        model::modified(info),
        model::permissions(info),
        model::file(info).parse_name()
    );
    let dialog = adw::AlertDialog::builder()
        .heading(info.display_name())
        .body(body)
        .build();
    dialog.add_response("close", "Close");
    dialog.set_close_response("close");
    let size = gtk::Label::new(Some(&format!(
        "Size: {}",
        model::format_size(info.size().max(0) as u64)
    )));
    size.set_wrap(true);
    dialog.set_extra_child(Some(&size));
    if model::is_dir(info) {
        size.set_text("Calculating folder size…");
        let cancel = gio::Cancellable::new();
        dialog.connect_closed(glib::clone!(
            #[strong]
            cancel,
            move |_| cancel.cancel()
        ));
        model::file(info).measure_disk_usage_async(
            gio::FileMeasureFlags::APPARENT_SIZE,
            glib::Priority::LOW,
            Some(&cancel),
            None,
            glib::clone!(
                #[weak]
                size,
                move |r| match r {
                    Ok((bytes, dirs, files)) => size.set_text(&format!(
                        "Size: {} · {files} files · {dirs} folders",
                        model::format_size(bytes)
                    )),
                    Err(e) =>
                        if !ops::is_cancelled(&e) {
                            size.set_text("Size unavailable");
                            report(e.to_string());
                        },
                }
            ),
        );
    }
    dialog.present(Some(parent));
}
