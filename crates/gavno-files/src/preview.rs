use crate::{
    clipboard,
    icons::IconCache,
    model::{self, FileEntry},
    thumbs,
    views::Report,
};
use adw::prelude::*;
use gtk::gdk;
use std::{
    cell::{Cell, RefCell},
    fs,
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub type ChmodFn = Rc<dyn Fn(PathBuf, u32, Box<dyn FnOnce(bool)>)>;

pub fn show_quicklook(
    parent: &adw::ApplicationWindow,
    entries: Vec<FileEntry>,
    start_index: usize,
    report: Report,
    on_close: Rc<dyn Fn()>,
) {
    if entries.is_empty() {
        return;
    }
    let total_entries = entries.len();
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(720)
        .default_height(520)
        .build();
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("", "");
    header.set_title_widget(Some(&title));

    let nav_box = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    nav_box.add_css_class("linked");
    let prev_btn = gtk::Button::from_icon_name("go-previous-symbolic");
    prev_btn.set_tooltip_text(Some("Previous file (Left)"));
    let next_btn = gtk::Button::from_icon_name("go-next-symbolic");
    next_btn.set_tooltip_text(Some("Next file (Right)"));
    nav_box.append(&prev_btn);
    nav_box.append(&next_btn);
    header.pack_start(&nav_box);
    layout.append(&header);

    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .build();
    let metadata = gtk::Label::new(None);
    metadata.set_wrap(true);
    metadata.set_margin_top(10);
    metadata.set_margin_bottom(10);
    metadata.set_margin_start(14);
    metadata.set_margin_end(14);
    metadata.add_css_class("dim-label");
    layout.append(&scroll);
    layout.append(&metadata);
    window.set_child(Some(&layout));

    let index = Rc::new(Cell::new(start_index.min(entries.len() - 1)));
    let cancel: Rc<RefCell<Arc<AtomicBool>>> = Rc::new(RefCell::new(Arc::new(AtomicBool::new(false))));

    window.connect_destroy(glib::clone!(
        #[strong]
        cancel,
        #[strong]
        on_close,
        move |_| {
            cancel.borrow().store(true, Ordering::Relaxed);
            on_close();
        }
    ));

    let load_current: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[weak]
        window,
        #[weak]
        title,
        #[weak]
        scroll,
        #[weak]
        metadata,
        #[weak]
        prev_btn,
        #[weak]
        next_btn,
        #[strong]
        index,
        #[strong]
        cancel,
        #[strong]
        report,
        move || {
            cancel.borrow().store(true, Ordering::Relaxed);
            let run_cancel = Arc::new(AtomicBool::new(false));
            cancel.replace(run_cancel.clone());

            let idx = index.get();
            let entry = &entries[idx];
            window.set_title(Some(&entry.display_name));
            title.set_title(&entry.display_name);
            title.set_subtitle(&entry.path.to_string_lossy());
            prev_btn.set_sensitive(idx > 0);
            next_btn.set_sensitive(idx + 1 < entries.len());

            metadata.set_text(&format!(
                "{} · {} · {} · {}",
                entry.kind_text,
                entry.size_text,
                entry.mtime_text,
                model::permissions_text(entry.is_dir, entry.mode)
            ));

            let spinner = gtk::Spinner::new();
            spinner.set_halign(gtk::Align::Center);
            spinner.set_valign(gtk::Align::Center);
            spinner.start();
            scroll.set_child(Some(&spinner));

            let entry_clone = entry.clone();
            let report = report.clone();
            glib::spawn_future_local(glib::clone!(
                #[weak]
                scroll,
                #[weak]
                metadata,
                async move {
                    if entry_clone.is_dir {
                        let path = entry_clone.path.clone();
                        let c = run_cancel.clone();
                        let Ok(summary) =
                            gio::spawn_blocking(move || summarize_dir_sync(&path, &c)).await
                        else {
                            return;
                        };
                        if run_cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        match summary {
                            Ok((items, total_bytes, n_dirs, n_files)) => {
                                metadata.set_text(&format!(
                                    "Folder · {} ({n_files} files, {n_dirs} folders) · {}",
                                    model::format_size(total_bytes),
                                    entry_clone.mtime_text
                                ));
                                let vbox = gtk::Box::new(gtk::Orientation::Vertical, 4);
                                vbox.set_margin_top(14);
                                vbox.set_margin_bottom(14);
                                vbox.set_margin_start(18);
                                vbox.set_margin_end(18);
                                if items.is_empty() {
                                    let empty = gtk::Label::new(Some("Empty folder"));
                                    empty.add_css_class("dim-label");
                                    vbox.append(&empty);
                                } else {
                                    for name in items {
                                        let lbl = gtk::Label::new(Some(&name));
                                        lbl.set_xalign(0.);
                                        lbl.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                                        vbox.append(&lbl);
                                    }
                                }
                                scroll.set_child(Some(&vbox));
                            }
                            Err(e) => {
                                let status = adw::StatusPage::builder()
                                    .icon_name("folder-symbolic")
                                    .title("Cannot read folder")
                                    .description(&e)
                                    .build();
                                scroll.set_child(Some(&status));
                            }
                        }
                        return;
                    }

                    if thumbs::supported_name(&entry_clone.name) {
                        let path = entry_clone.path.clone();
                        let res = gio::spawn_blocking(move || decode_full_image(&path)).await;
                        if run_cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        match res {
                            Ok(Ok((tex, w, h))) => {
                                metadata.set_text(&format!(
                                    "{} · {}×{} px · {} · {}",
                                    entry_clone.kind_text,
                                    w,
                                    h,
                                    entry_clone.size_text,
                                    entry_clone.mtime_text
                                ));
                                let picture = gtk::Picture::for_paintable(&tex);
                                picture.set_can_shrink(true);
                                scroll.set_child(Some(&picture));
                                return;
                            }
                            Ok(Err(e)) => report(e),
                            Err(_) => {}
                        }
                    }

                    // Try reading text up to 128 KiB (covers Makefiles, source files, logs even without MIME).
                    let path = entry_clone.path.clone();
                    let force_text = model::is_textual_mime(&entry_clone.content_type);
                    let res = gio::spawn_blocking(move || read_text_preview(&path, force_text)).await;
                    if run_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    match res {
                        Ok(Ok(Some(text))) => {
                            let view = gtk::TextView::new();
                            view.set_editable(false);
                            view.set_cursor_visible(false);
                            view.set_monospace(true);
                            view.set_left_margin(16);
                            view.set_right_margin(16);
                            view.set_top_margin(12);
                            view.set_bottom_margin(12);
                            view.buffer().set_text(&text);
                            scroll.set_child(Some(&view));
                        }
                        Ok(Ok(None)) => {
                            let status = adw::StatusPage::builder()
                                .icon_name("text-x-generic-symbolic")
                                .title("Binary file — no preview")
                                .description("Preview is unavailable for binary or special files.")
                                .build();
                            scroll.set_child(Some(&status));
                        }
                        Ok(Err(e)) => {
                            let status = adw::StatusPage::builder()
                                .icon_name("dialog-error-symbolic")
                                .title("Cannot read file")
                                .description(&e)
                                .build();
                            scroll.set_child(Some(&status));
                        }
                        Err(_) => {}
                    }
                }
            ));
        }
    ));

    load_current();

    prev_btn.connect_clicked(glib::clone!(
        #[strong]
        index,
        #[strong]
        load_current,
        move |_| {
            let cur = index.get();
            if cur > 0 {
                index.set(cur - 1);
                load_current();
            }
        }
    ));
    next_btn.connect_clicked(glib::clone!(
        #[strong]
        index,
        #[strong]
        load_current,
        move |_| {
            let cur = index.get();
            if cur + 1 < total_entries {
                index.set(cur + 1);
                load_current();
            }
        }
    ));

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        window,
        #[weak]
        prev_btn,
        #[weak]
        next_btn,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            let in_text = GtkWindowExt::focus(&window)
                .is_some_and(|w| w.is::<gtk::TextView>() || w.ancestor(gtk::TextView::static_type()).is_some());
            match key {
                gdk::Key::space | gdk::Key::Escape => {
                    window.close();
                    glib::Propagation::Stop
                }
                gdk::Key::Left => {
                    if prev_btn.is_sensitive() {
                        prev_btn.emit_clicked();
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::Up if !in_text => {
                    if prev_btn.is_sensitive() {
                        prev_btn.emit_clicked();
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::Right => {
                    if next_btn.is_sensitive() {
                        next_btn.emit_clicked();
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::Down if !in_text => {
                    if next_btn.is_sensitive() {
                        next_btn.emit_clicked();
                    }
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }
    ));
    window.add_controller(keys);
    window.present();
}

struct ImageDecodeResult {
    data: Vec<u8>,
    width: i32,
    height: i32,
    stride: usize,
    alpha: bool,
    orig_w: i32,
    orig_h: i32,
}

fn decode_full_image(path: &Path) -> Result<(gdk::Texture, i32, i32), String> {
    let info = gdk_pixbuf::Pixbuf::file_info(path).map(|(_, w, h)| (w, h));
    let (mut orig_w, mut orig_h) = info.unwrap_or((0, 0));
    let original = gdk_pixbuf::Pixbuf::from_file_at_scale(path, 1400, 1000, true)
        .map_err(|e| e.to_string())?;
    let (raw_w, raw_h) = (original.width(), original.height());
    let p = original.apply_embedded_orientation().unwrap_or(original);
    if raw_w != raw_h && ((raw_w > raw_h) != (p.width() > p.height())) {
        std::mem::swap(&mut orig_w, &mut orig_h);
    }
    let res = ImageDecodeResult {
        data: p.read_pixel_bytes().to_vec(),
        width: p.width(),
        height: p.height(),
        stride: p.rowstride() as usize,
        alpha: p.has_alpha(),
        orig_w: if orig_w > 0 { orig_w } else { p.width() },
        orig_h: if orig_h > 0 { orig_h } else { p.height() },
    };
    let tex: gdk::Texture = gdk::MemoryTexture::new(
        res.width,
        res.height,
        if res.alpha {
            gdk::MemoryFormat::R8g8b8a8
        } else {
            gdk::MemoryFormat::R8g8b8
        },
        &glib::Bytes::from_owned(res.data),
        res.stride,
    )
    .upcast();
    Ok((tex, res.orig_w, res.orig_h))
}

fn read_text_preview(path: &Path, force_text: bool) -> Result<Option<String>, String> {
    const MAX: usize = 128 * 1024;
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.file_type().is_file() {
        return Ok(None);
    }
    let mut f = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(8192);
    let mut handle = (&mut f).take(MAX as u64);
    handle.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let sniff_len = buf.len().min(4096);
    if !force_text && buf[..sniff_len].contains(&0) {
        return Ok(None);
    }
    let mut text = String::from_utf8_lossy(&buf).replace('\0', "");
    if buf.len() == MAX {
        text.push_str("\n\n[Preview limited to 128 KiB]");
    }
    Ok(Some(text))
}

fn summarize_dir_sync(
    root: &Path,
    cancel: &AtomicBool,
) -> Result<(Vec<String>, u64, usize, usize), String> {
    let mut items = Vec::new();
    let rd = fs::read_dir(root).map_err(|e| e.to_string())?;
    for entry in rd.flatten().take(24) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        items.push(if is_dir { format!("📁 {name}") } else { name });
    }
    items.sort();
    let (bytes, dirs, files) = measure_dir_sync(root, cancel)?;
    Ok((items, bytes, dirs, files))
}

pub fn measure_dir_sync(root: &Path, cancel: &AtomicBool) -> Result<(u64, usize, usize), String> {
    let mut stack = vec![root.to_path_buf()];
    let mut bytes = 0u64;
    let mut dirs = 0usize;
    let mut files = 0usize;
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".to_owned());
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return Err("Cancelled".to_owned());
            }
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                files += 1;
            } else if meta.is_dir() {
                dirs += 1;
                stack.push(entry.path());
            } else {
                files += 1;
                bytes = bytes.saturating_add(meta.len());
            }
        }
    }
    Ok((bytes, dirs, files))
}

pub fn show_properties(
    parent: &adw::ApplicationWindow,
    entry: FileEntry,
    icon_cache: &IconCache,
    can_chmod: bool,
    on_chmod: ChmodFn,
) {
    let window = adw::Window::builder()
        .title(format!("{} — Properties", entry.display_name))
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .default_height(520)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    root.append(&header);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    body.set_margin_top(14);
    body.set_margin_bottom(18);
    body.set_margin_start(18);
    body.set_margin_end(18);

    // Top summary row.
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    let icon = gtk::Image::from_paintable(Some(&icon_cache.texture(&entry.icon, 48)));
    icon.set_pixel_size(48);
    top.append(&icon);
    let top_text = gtk::Box::new(gtk::Orientation::Vertical, 3);
    let name_lbl = gtk::Label::new(Some(&entry.display_name));
    name_lbl.set_xalign(0.);
    name_lbl.set_wrap(true);
    name_lbl.add_css_class("title-3");
    let kind_lbl = gtk::Label::new(Some(&format!("{} ({})", entry.kind_text, entry.content_type)));
    kind_lbl.set_xalign(0.);
    kind_lbl.add_css_class("dim-label");
    top_text.append(&name_lbl);
    top_text.append(&kind_lbl);
    top.append(&top_text);
    body.append(&top);

    // Details group.
    let group = adw::PreferencesGroup::new();
    let loc_row = adw::ActionRow::builder()
        .title("Location")
        .subtitle(entry.path.to_string_lossy().as_ref())
        .use_markup(false)
        .build();
    let copy_btn = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy_btn.add_css_class("flat");
    copy_btn.set_valign(gtk::Align::Center);
    copy_btn.set_tooltip_text(Some("Copy path"));
    let path_str = entry.path.to_string_lossy().into_owned();
    copy_btn.connect_clicked(glib::clone!(
        #[weak]
        window,
        move |_| {
            clipboard::copy_text(&window.clipboard(), &path_str);
        }
    ));
    loc_row.add_suffix(&copy_btn);
    group.add(&loc_row);

    if let Some(ref target) = entry.symlink_target {
        group.add(
            &adw::ActionRow::builder()
                .title("Link Target")
                .subtitle(target.as_str())
                .use_markup(false)
                .build(),
        );
    }

    let size_row = adw::ActionRow::builder()
        .title("Size")
        .subtitle(if entry.is_dir && entry.trash.is_none() {
            "Calculating…"
        } else {
            &entry.size_text
        })
        .use_markup(false)
        .build();
    group.add(&size_row);

    group.add(
        &adw::ActionRow::builder()
            .title("Modified")
            .subtitle(&entry.mtime_text)
            .use_markup(false)
            .build(),
    );
    group.add(
        &adw::ActionRow::builder()
            .title("Owner")
            .subtitle(&entry.owner_text)
            .use_markup(false)
            .build(),
    );
    let perm_row = adw::ActionRow::builder()
        .title("Permissions")
        .subtitle(&model::permissions_text(entry.is_dir, entry.mode))
        .use_markup(false)
        .build();
    group.add(&perm_row);
    body.append(&group);

    // Interactive permissions grid.
    if entry.trash.is_none() && !entry.is_symlink {
        let perm_group = adw::PreferencesGroup::builder()
            .title("Access Mode")
            .build();
        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(14);
        grid.set_margin_top(6);
        grid.set_margin_bottom(6);

        for (col, title) in [(1, "Read"), (2, "Write"), (3, "Execute")] {
            let lbl = gtk::Label::new(Some(title));
            lbl.add_css_class("dim-label");
            grid.attach(&lbl, col, 0, 1, 1);
        }

        let current_mode = Rc::new(Cell::new(entry.mode & 0o7777));
        let committed_mode = Rc::new(Cell::new(entry.mode & 0o7777));
        let chmod_seq = Rc::new(Cell::new(0u64));
        let updating = Rc::new(Cell::new(false));
        let boxes: Rc<RefCell<Vec<(u32, gtk::CheckButton)>>> = Rc::new(RefCell::new(Vec::new()));
        let path_for_chmod = entry.path.clone();

        for (row_idx, (who, shift)) in [("Owner", 6u32), ("Group", 3), ("Others", 0)]
            .into_iter()
            .enumerate()
        {
            let lbl = gtk::Label::new(Some(who));
            lbl.set_xalign(0.);
            grid.attach(&lbl, 0, row_idx as i32 + 1, 1, 1);

            for (bit_idx, bit) in [4u32, 2, 1].into_iter().enumerate() {
                let mask = bit << shift;
                let cb = gtk::CheckButton::new();
                cb.set_active((current_mode.get() & mask) != 0);
                cb.set_sensitive(can_chmod);
                cb.set_halign(gtk::Align::Center);
                boxes.borrow_mut().push((mask, cb.clone()));
                cb.connect_toggled(glib::clone!(
                    #[weak]
                    perm_row,
                    #[strong]
                    current_mode,
                    #[strong]
                    committed_mode,
                    #[strong]
                    chmod_seq,
                    #[strong]
                    updating,
                    #[strong]
                    boxes,
                    #[strong]
                    path_for_chmod,
                    #[strong]
                    on_chmod,
                    move |btn| {
                        if updating.get() {
                            return;
                        }
                        let mut m = current_mode.get();
                        if btn.is_active() {
                            m |= mask;
                        } else {
                            m &= !mask;
                        }
                        let seq = chmod_seq.get().wrapping_add(1);
                        chmod_seq.set(seq);
                        current_mode.set(m);
                        perm_row.set_subtitle(&model::permissions_text(entry.is_dir, m));
                        for (_, b) in boxes.borrow().iter() {
                            b.set_sensitive(false);
                        }
                        let perm_weak = perm_row.downgrade();
                        let cur_ref = current_mode.clone();
                        let comm_ref = committed_mode.clone();
                        let seq_ref = chmod_seq.clone();
                        let upd_ref = updating.clone();
                        let boxes_ref = boxes.clone();
                        on_chmod(
                            path_for_chmod.clone(),
                            m,
                            Box::new(move |ok| {
                                if seq_ref.get() != seq {
                                    return;
                                }
                                if ok {
                                    comm_ref.set(m);
                                } else {
                                    let revert = comm_ref.get();
                                    upd_ref.set(true);
                                    cur_ref.set(revert);
                                    if let Some(pr) = perm_weak.upgrade() {
                                        pr.set_subtitle(&model::permissions_text(
                                            entry.is_dir,
                                            revert,
                                        ));
                                    }
                                    for (mask, b) in boxes_ref.borrow().iter() {
                                        b.set_active((revert & *mask) != 0);
                                    }
                                    upd_ref.set(false);
                                }
                                for (_, b) in boxes_ref.borrow().iter() {
                                    b.set_sensitive(can_chmod);
                                }
                            }),
                        );
                    }
                ));
                grid.attach(&cb, bit_idx as i32 + 1, row_idx as i32 + 1, 1, 1);
            }
        }
        perm_group.add(&grid);
        body.append(&perm_group);
    }

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&body)
        .build();
    root.append(&scroll);
    window.set_content(Some(&root));

    let cancel = Arc::new(AtomicBool::new(false));
    window.connect_destroy(glib::clone!(
        #[strong]
        cancel,
        move |_| cancel.store(true, Ordering::Relaxed)
    ));

    if entry.is_dir && entry.trash.is_none() {
        let dir = entry.path.clone();
        let c = cancel.clone();
        glib::spawn_future_local(glib::clone!(
            #[weak]
            size_row,
            async move {
                if let Ok(Ok((bytes, dirs, files))) =
                    gio::spawn_blocking(move || measure_dir_sync(&dir, &c)).await
                {
                    size_row.set_subtitle(&format!(
                        "{} · {files} files · {dirs} folders",
                        model::format_size(bytes)
                    ));
                }
            }
        ));
    }

    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        window,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    ));
    window.add_controller(keys);
    window.present();
}
