use crate::model;
use adw::prelude::*;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::Path,
    rc::Rc,
};

fn cancelled() -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::Cancelled, "Operation cancelled")
}
pub fn is_cancelled(e: &glib::Error) -> bool {
    e.matches(gio::IOErrorEnum::Cancelled)
}
pub async fn enumerate(
    file: &gio::File,
    attrs: &str,
    cancel: &gio::Cancellable,
) -> Result<gio::FileEnumerator, glib::Error> {
    enumerate_flags(
        file,
        attrs,
        cancel,
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
    )
    .await
}
pub async fn enumerate_display(
    file: &gio::File,
    attrs: &str,
    cancel: &gio::Cancellable,
) -> Result<gio::FileEnumerator, glib::Error> {
    enumerate_flags(file, attrs, cancel, gio::FileQueryInfoFlags::NONE).await
}
async fn enumerate_flags(
    file: &gio::File,
    attrs: &str,
    cancel: &gio::Cancellable,
    flags: gio::FileQueryInfoFlags,
) -> Result<gio::FileEnumerator, glib::Error> {
    let (send, receive) = futures_channel::oneshot::channel();
    file.enumerate_children_async(
        attrs,
        flags,
        glib::Priority::DEFAULT,
        Some(cancel),
        move |r| {
            let _ = send.send(r);
        },
    );
    receive.await.unwrap_or_else(|_| Err(cancelled()))
}
pub async fn next(
    enumerator: &gio::FileEnumerator,
    cancel: &gio::Cancellable,
) -> Result<Vec<gio::FileInfo>, glib::Error> {
    let (send, receive) = futures_channel::oneshot::channel();
    enumerator.next_files_async(
        model::BATCH,
        glib::Priority::DEFAULT,
        Some(cancel),
        move |r| {
            let _ = send.send(r);
        },
    );
    receive.await.unwrap_or_else(|_| Err(cancelled()))
}
pub async fn query(
    file: &gio::File,
    cancel: &gio::Cancellable,
) -> Result<gio::FileInfo, glib::Error> {
    let (send, receive) = futures_channel::oneshot::channel();
    file.query_info_async(
        model::ATTRIBUTES,
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
        glib::Priority::DEFAULT,
        Some(cancel),
        move |r| {
            let _ = send.send(r);
        },
    );
    receive.await.unwrap_or_else(|_| Err(cancelled()))
}
pub async fn children(
    file: &gio::File,
    cancel: &gio::Cancellable,
) -> Result<Vec<gio::FileInfo>, glib::Error> {
    let e = enumerate(file, model::ATTRIBUTES, cancel).await?;
    let mut out = Vec::new();
    loop {
        let batch = next(&e, cancel).await?;
        if batch.is_empty() {
            break;
        }
        for info in batch {
            model::attach(&info, &e.child(&info));
            out.push(info);
        }
    }
    e.close_future(glib::Priority::DEFAULT).await?;
    Ok(out)
}

pub fn copy_name(name: &str, number: u32, is_folder: bool) -> String {
    let (stem, ext) = if !is_folder {
        match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
            _ => (name, String::new()),
        }
    } else {
        (name, String::new())
    };
    if number == 1 {
        format!("{stem} (copy){ext}")
    } else {
        format!("{stem} (copy {number}){ext}")
    }
}

#[derive(Clone)]
pub struct Operations {
    pub button: gtk::MenuButton,
    rows: gtk::Box,
    active: Rc<RefCell<Vec<gio::Cancellable>>>,
}
#[derive(Clone)]
pub struct Operation {
    pub cancel: gio::Cancellable,
    bar: gtk::ProgressBar,
    label: gtk::Label,
    row: gtk::Box,
    owner: Operations,
    pub completed: Rc<Cell<usize>>,
}
impl Operations {
    pub fn new() -> Self {
        let button = gtk::MenuButton::builder()
            .icon_name("view-list-bullet-symbolic")
            .tooltip_text("File operations")
            .build();
        let rows = gtk::Box::new(gtk::Orientation::Vertical, 12);
        rows.set_margin_top(12);
        rows.set_margin_bottom(12);
        rows.set_margin_start(12);
        rows.set_margin_end(12);
        rows.set_width_request(300);
        let popover = gtk::Popover::new();
        popover.set_child(Some(&rows));
        button.set_popover(Some(&popover));
        button.set_sensitive(false);
        Self {
            button,
            rows,
            active: Rc::new(RefCell::new(Vec::new())),
        }
    }
    pub fn cancel_all(&self) {
        for cancel in self.active.borrow().iter() {
            cancel.cancel();
        }
    }
    pub fn start(&self, title: &str) -> Operation {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let label = gtk::Label::new(Some(title));
        label.set_hexpand(true);
        label.set_xalign(0.);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let stop = gtk::Button::from_icon_name("process-stop-symbolic");
        stop.set_tooltip_text(Some("Cancel operation"));
        let cancel = gio::Cancellable::new();
        self.active.borrow_mut().push(cancel.clone());
        stop.connect_clicked(glib::clone!(
            #[strong]
            cancel,
            move |_| cancel.cancel()
        ));
        top.append(&label);
        top.append(&stop);
        let bar = gtk::ProgressBar::new();
        row.append(&top);
        row.append(&bar);
        self.rows.append(&row);
        self.button.set_sensitive(true);
        Operation {
            cancel,
            bar,
            label,
            row,
            owner: self.clone(),
            completed: Rc::new(Cell::new(0)),
        }
    }
}
impl Operation {
    pub fn progress(&self, name: &str, current: i64, total: i64) {
        self.label
            .set_text(&format!("{} · {name}", self.completed.get()));
        if total > 0 {
            self.bar
                .set_fraction((current as f64 / total as f64).clamp(0., 1.));
        } else {
            self.bar.pulse();
        }
    }
    pub fn finish(&self) {
        self.owner.active.borrow_mut().retain(|c| c != &self.cancel);
        self.owner.rows.remove(&self.row);
        self.owner
            .button
            .set_sensitive(self.owner.rows.first_child().is_some());
    }
}

pub async fn confirm(
    parent: &adw::ApplicationWindow,
    heading: &str,
    body: &str,
    label: &str,
) -> bool {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("yes", label)]);
    dialog.set_close_response("cancel");
    dialog.set_default_response(Some("cancel"));
    dialog.set_response_appearance("yes", adw::ResponseAppearance::Destructive);
    dialog.choose_future(Some(parent)).await == "yes"
}

async fn destination(
    parent: &adw::ApplicationWindow,
    source: &gio::File,
    target: gio::File,
    is_folder: bool,
    cancel: &gio::Cancellable,
) -> Result<Option<gio::File>, glib::Error> {
    match query(&target, cancel).await {
        Err(e) if e.matches(gio::IOErrorEnum::NotFound) => return Ok(Some(target)),
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    let name = target
        .basename()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dialog = adw::AlertDialog::builder()
        .heading("A file with this name already exists")
        .body(format!(
            "“{name}” already exists in the destination. Replacing a folder removes its contents."
        ))
        .build();
    dialog.add_responses(&[
        ("skip", "Skip"),
        ("both", "Keep Both"),
        ("replace", "Replace"),
    ]);
    dialog.set_close_response("skip");
    dialog.set_default_response(Some("both"));
    dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
    match dialog.choose_future(Some(parent)).await.as_str() {
        "replace" if source != &target && !source.has_prefix(&target) => {
            delete_tree(&target, cancel).await?;
            Ok(Some(target))
        }
        "replace" => Err(glib::Error::new(
            gio::IOErrorEnum::InvalidArgument,
            "Cannot replace a file with itself or replace one of its parent folders",
        )),
        "both" => {
            let Some(folder) = target.parent() else {
                return Ok(None);
            };
            for n in 1..u32::MAX {
                let candidate = folder.child(copy_name(&name, n, is_folder));
                match query(&candidate, cancel).await {
                    Err(e) if e.matches(gio::IOErrorEnum::NotFound) => return Ok(Some(candidate)),
                    Err(e) => return Err(e),
                    Ok(_) => {}
                }
            }
            Err(glib::Error::new(
                gio::IOErrorEnum::Exists,
                "No available copy name",
            ))
        }
        _ => Ok(None),
    }
}

async fn move_file(
    source: &gio::File,
    target: &gio::File,
    info: &gio::FileInfo,
    op: &Operation,
) -> Result<(), glib::Error> {
    let src = source.clone();
    let dest = target.clone();
    let cancel = op.cancel.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let progress = op.clone();
    let name = info.display_name().to_string();
    let timer = glib::timeout_add_local(std::time::Duration::from_millis(80), move || {
        if let Some((current, total)) = receive.try_iter().last() {
            progress.progress(&name, current, total);
        }
        glib::ControlFlow::Continue
    });
    let result = gio::spawn_blocking(move || {
        src.move_(
            &dest,
            gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
            Some(&cancel),
            Some(&mut |c, t| {
                let _ = send.send((c, t));
            }),
        )
    })
    .await;
    timer.remove();
    result.map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "Move worker failed"))?
}

async fn check_destination(source: &gio::File, target: &gio::File) -> Result<(), glib::Error> {
    if target == source || target.has_prefix(source) {
        return Err(glib::Error::new(
            gio::IOErrorEnum::WouldRecurse,
            "Cannot copy or move a folder into itself",
        ));
    }
    if let (Some(source), Some(parent)) = (source.path(), target.parent().and_then(|p| p.path())) {
        gio::spawn_blocking(move || {
            let source = std::fs::canonicalize(source)
                .map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &e.to_string()))?;
            let parent = std::fs::canonicalize(parent)
                .map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &e.to_string()))?;
            if parent.starts_with(source) {
                Err(glib::Error::new(
                    gio::IOErrorEnum::WouldRecurse,
                    "The destination links to the source folder",
                ))
            } else {
                Ok(())
            }
        })
        .await
        .map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "Location worker failed"))??;
    }
    Ok(())
}

pub async fn transfer(
    parent: &adw::ApplicationWindow,
    sources: Vec<gio::File>,
    folder: gio::File,
    moving: bool,
    op: &Operation,
) -> Result<bool, glib::Error> {
    let mut queue = VecDeque::new();
    let mut cleanup = Vec::new();
    let mut all = true;
    for source in sources {
        if let Some(name) = source.basename() {
            queue.push_back((source, folder.child(name)));
        }
    }
    while let Some((source, target)) = queue.pop_front() {
        op.cancel.set_error_if_cancelled()?;
        let info = query(&source, &op.cancel).await?;
        let dir = model::is_dir(&info);
        if moving && source == target {
            continue;
        }
        // Reject descendants before presenting a replacement dialog; keep-both at
        // the same location is allowed and checked again after choosing its name.
        if dir && target != source {
            check_destination(&source, &target).await?;
        }
        let Some(target) = destination(parent, &source, target, dir, &op.cancel).await? else {
            all = false;
            continue;
        };
        if dir {
            check_destination(&source, &target).await?;
        }
        if moving {
            match move_file(&source, &target, &info, op).await {
                Ok(()) => {
                    op.completed.set(op.completed.get() + 1);
                    continue;
                }
                Err(e)
                    if dir
                        && (e.matches(gio::IOErrorEnum::WouldRecurse)
                            || e.matches(gio::IOErrorEnum::NotSupported)) => {}
                Err(e) => return Err(e),
            }
        }
        if dir {
            let dest = target.clone();
            let cancel = op.cancel.clone();
            gio::spawn_blocking(move || dest.make_directory(Some(&cancel)))
                .await
                .map_err(|_| {
                    glib::Error::new(gio::IOErrorEnum::Failed, "Folder worker failed")
                })??;
            let e = enumerate(&source, model::ATTRIBUTES, &op.cancel).await?;
            loop {
                let batch = next(&e, &op.cancel).await?;
                if batch.is_empty() {
                    break;
                }
                for child in batch {
                    queue.push_back((e.child(&child), target.child(child.name())));
                }
            }
            e.close_future(glib::Priority::DEFAULT).await?;
            if moving {
                cleanup.push(source);
            }
        } else {
            let (send, receive) = futures_channel::oneshot::channel();
            let progress = op.clone();
            let name = info.display_name().to_string();
            source.copy_async(
                &target,
                gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
                glib::Priority::DEFAULT,
                Some(&op.cancel),
                Some(Box::new(move |c, t| progress.progress(&name, c, t))),
                move |r| {
                    let _ = send.send(r);
                },
            );
            receive.await.unwrap_or_else(|_| Err(cancelled()))?;
        }
        op.completed.set(op.completed.get() + 1);
    }
    for source in cleanup.into_iter().rev() {
        let (send, receive) = futures_channel::oneshot::channel();
        source.delete_async(glib::Priority::DEFAULT, Some(&op.cancel), move |r| {
            let _ = send.send(r);
        });
        match receive.await.unwrap_or_else(|_| Err(cancelled())) {
            Ok(()) => {}
            Err(e) if e.matches(gio::IOErrorEnum::NotEmpty) => all = false,
            Err(e) => return Err(e),
        }
    }
    Ok(all)
}

pub async fn delete_tree(root: &gio::File, cancel: &gio::Cancellable) -> Result<(), glib::Error> {
    let mut stack = vec![(root.clone(), false)];
    while let Some((file, visited)) = stack.pop() {
        cancel.set_error_if_cancelled()?;
        if !visited && model::is_dir(&query(&file, cancel).await?) {
            stack.push((file.clone(), true));
            let e = enumerate(&file, model::ATTRIBUTES, cancel).await?;
            loop {
                let batch = next(&e, cancel).await?;
                if batch.is_empty() {
                    break;
                }
                for info in batch {
                    stack.push((e.child(&info), false));
                }
            }
            e.close_future(glib::Priority::DEFAULT).await?;
        } else {
            let (send, receive) = futures_channel::oneshot::channel();
            file.delete_async(glib::Priority::DEFAULT, Some(cancel), move |r| {
                let _ = send.send(r);
            });
            receive.await.unwrap_or_else(|_| Err(cancelled()))?;
        }
    }
    Ok(())
}
pub async fn trash(file: &gio::File, cancel: &gio::Cancellable) -> Result<(), glib::Error> {
    let (send, receive) = futures_channel::oneshot::channel();
    file.trash_async(glib::Priority::DEFAULT, Some(cancel), move |r| {
        let _ = send.send(r);
    });
    receive.await.unwrap_or_else(|_| Err(cancelled()))
}
pub async fn restore(
    parent: &adw::ApplicationWindow,
    info: &gio::FileInfo,
    cancel: &gio::Cancellable,
) -> Result<(), glib::Error> {
    let path = info
        .attribute_byte_string("trash::orig-path")
        .ok_or_else(|| {
            glib::Error::new(
                gio::IOErrorEnum::NotFound,
                "Original location is unavailable",
            )
        })?;
    let source = model::file(info);
    let target = gio::File::for_path(Path::new(path.as_str()));
    let Some(target) = destination(parent, &source, target, model::is_dir(info), cancel).await?
    else {
        return Ok(());
    };
    let cancel = cancel.clone();
    gio::spawn_blocking(move || {
        source.move_(
            &target,
            gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
            Some(&cancel),
            None,
        )
    })
    .await
    .map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "Restore worker failed"))?
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conflict_names() {
        assert_eq!(copy_name("a.txt", 1, false), "a (copy).txt");
        assert_eq!(copy_name("a.txt", 2, false), "a (copy 2).txt");
        assert_eq!(copy_name(".bashrc", 1, false), ".bashrc (copy)");
        assert_eq!(
            copy_name("archive.tar.gz", 1, false),
            "archive.tar (copy).gz"
        );
        assert_eq!(copy_name("folder.ext", 2, true), "folder.ext (copy 2)");
        assert_eq!(copy_name("README", 1, false), "README (copy)");
    }
}
