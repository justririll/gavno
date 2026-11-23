use adw::prelude::*;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

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

pub fn unique_copy_target(folder: &Path, name: &str, is_folder: bool) -> Result<PathBuf, String> {
    for n in 1..u32::MAX {
        let candidate = folder.join(copy_name(name, n, is_folder));
        if fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err("No available copy name".to_owned())
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        if !matches!(c, Component::CurDir) {
            out.push(c.as_os_str());
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn canonical_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let clean = normalize_lexical(path);
    let mut cur = clean.as_path();
    let mut tail = Vec::new();
    loop {
        if let Ok(canon) = fs::canonicalize(cur) {
            let mut res = canon;
            for part in tail.into_iter().rev() {
                res.push(part);
            }
            return Some(res);
        }
        let name = cur.file_name()?;
        tail.push(name.to_os_string());
        cur = cur.parent()?;
    }
}

fn canonical_entry_path(path: &Path) -> Option<PathBuf> {
    let clean = normalize_lexical(path);
    if clean == Path::new("/") {
        return Some(clean);
    }
    let name = clean.file_name()?;
    let parent = match clean.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    Some(canonical_with_missing_tail(parent)?.join(name))
}

fn same_entry(a: &Path, b: &Path) -> bool {
    if normalize_lexical(a) == normalize_lexical(b) {
        return true;
    }
    matches!(
        (canonical_entry_path(a), canonical_entry_path(b)),
        (Some(ca), Some(cb)) if ca == cb
    )
}

fn is_descendant_of_real_dir(candidate: &Path, ancestor_dir: &Path) -> bool {
    let Ok(anc_meta) = fs::symlink_metadata(ancestor_dir) else {
        return false;
    };
    if anc_meta.file_type().is_symlink() || !anc_meta.is_dir() {
        return false;
    }
    let anc_id = (anc_meta.dev(), anc_meta.ino());
    let clean = normalize_lexical(candidate);
    let mut cur = Some(clean.as_path());
    while let Some(p) = cur {
        if let Ok(m) = fs::symlink_metadata(p) {
            if !m.file_type().is_symlink() && (m.dev(), m.ino()) == anc_id {
                return true;
            }
        }
        cur = p.parent();
    }
    let Ok(anc_canon) = fs::canonicalize(ancestor_dir) else {
        return false;
    };
    let Some(cand_canon) = canonical_entry_path(candidate) else {
        return false;
    };
    cand_canon == anc_canon || cand_canon.starts_with(&anc_canon)
}

fn is_path_prefix_entry(source: &Path, target: &Path) -> bool {
    let clean = normalize_lexical(source);
    let mut cur = clean.parent();
    while let Some(anc) = cur {
        if anc.as_os_str().is_empty() {
            break;
        }
        if same_entry(anc, target) {
            return true;
        }
        cur = anc.parent();
    }
    false
}

fn is_self_or_ancestor_conflict(source: &Path, target: &Path) -> bool {
    same_entry(source, target)
        || is_descendant_of_real_dir(source, target)
        || is_path_prefix_entry(source, target)
}

#[derive(Clone)]
pub struct Operations {
    pub button: gtk::MenuButton,
    rows: gtk::Box,
    active: Rc<RefCell<Vec<Arc<AtomicBool>>>>,
}

#[derive(Clone)]
pub struct Operation {
    pub cancel: Arc<AtomicBool>,
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
        button.set_visible(false);
        Self {
            button,
            rows,
            active: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub fn cancel_all(&self) {
        for flag in self.active.borrow().iter() {
            flag.store(true, Ordering::Relaxed);
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
        stop.add_css_class("flat");
        stop.set_tooltip_text(Some("Cancel operation"));
        let cancel = Arc::new(AtomicBool::new(false));
        self.active.borrow_mut().push(cancel.clone());
        stop.connect_clicked(glib::clone!(
            #[strong]
            cancel,
            move |_| {
                cancel.store(true, Ordering::Relaxed);
            }
        ));
        top.append(&label);
        top.append(&stop);
        let bar = gtk::ProgressBar::new();
        row.append(&top);
        row.append(&bar);
        self.rows.append(&row);
        self.button.set_visible(true);
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
        self.owner
            .active
            .borrow_mut()
            .retain(|c| !Arc::ptr_eq(c, &self.cancel));
        self.owner.rows.remove(&self.row);
        self.owner
            .button
            .set_visible(self.owner.rows.first_child().is_some());
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictAction {
    Replace,
    Skip,
    KeepBoth,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    Ask,
    All(ConflictAction),
}

async fn resolve_conflict(
    parent: &adw::ApplicationWindow,
    source: &Path,
    target: &Path,
    is_folder: bool,
    policy: &mut ConflictPolicy,
) -> Result<Option<(PathBuf, Option<(PathBuf, PathBuf)>)>, String> {
    if fs::symlink_metadata(target).is_err() {
        return Ok(Some((target.to_path_buf(), None)));
    }
    let name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let action = match *policy {
        ConflictPolicy::All(act) => act,
        ConflictPolicy::Ask => {
            let dialog = adw::AlertDialog::builder()
                .heading("A file with this name already exists")
                .body(format!(
                    "“{name}” already exists in the destination. Replacing a folder removes its contents."
                ))
                .build();
            let apply_all = gtk::CheckButton::with_label("Apply to all remaining conflicts");
            dialog.set_extra_child(Some(&apply_all));
            dialog.add_responses(&[
                ("cancel", "Cancel"),
                ("skip", "Skip"),
                ("both", "Keep Both"),
                ("replace", "Replace"),
            ]);
            dialog.set_close_response("cancel");
            dialog.set_default_response(Some("both"));
            dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
            let choice = match dialog.choose_future(Some(parent)).await.as_str() {
                "replace" => ConflictAction::Replace,
                "both" => ConflictAction::KeepBoth,
                "skip" => ConflictAction::Skip,
                _ => ConflictAction::Cancel,
            };
            if apply_all.is_active() && choice != ConflictAction::Cancel {
                *policy = ConflictPolicy::All(choice);
            }
            choice
        }
    };

    match action {
        ConflictAction::Cancel => Err("Cancelled".to_owned()),
        ConflictAction::Skip => Ok(None),
        ConflictAction::KeepBoth => {
            let folder = target.parent().ok_or("No destination folder")?;
            let unique = unique_copy_target(folder, &name, is_folder)?;
            Ok(Some((unique, None)))
        }
        ConflictAction::Replace => {
            let s = source.to_path_buf();
            let t = target.to_path_buf();
            let backup = gio::spawn_blocking(move || {
                if is_self_or_ancestor_conflict(&s, &t) {
                    return Err(
                        "Cannot replace a file with itself or one of its parent folders".to_owned(),
                    );
                }
                let parent = t.parent().ok_or("No destination folder")?;
                let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
                let backup = parent.join(format!(".gavno-replace-{}-{seq}", std::process::id()));
                fs::rename(&t, &backup).map_err(|e| e.to_string())?;
                Ok(backup)
            })
            .await
            .map_err(|_| "Worker failed".to_owned())??;
            Ok(Some((target.to_path_buf(), Some((target.to_path_buf(), backup)))))
        }
    }
}

async fn restore_backup(target: PathBuf, backup: PathBuf) {
    let _ = gio::spawn_blocking(move || {
        let _ = remove_tree_sync(&target, &AtomicBool::new(false));
        let _ = fs::rename(&backup, &target);
    })
    .await;
}

async fn remove_backup(backup: PathBuf, cancel: Arc<AtomicBool>) {
    let _ = gio::spawn_blocking(move || remove_tree_sync(&backup, &cancel)).await;
}

fn check_destination_sync(source: &Path, target: &Path) -> Result<(), String> {
    if same_entry(source, target) || is_descendant_of_real_dir(target, source) {
        return Err("Cannot copy or move a folder into itself".to_owned());
    }
    Ok(())
}

pub struct TransferReport {
    pub completed: usize,
    pub skipped: usize,
    pub cancelled: bool,
    pub created_paths: Vec<PathBuf>,
}

async fn rollback_backups(deferred_backups: &mut Vec<(PathBuf, PathBuf)>) {
    while let Some((bt, bb)) = deferred_backups.pop() {
        restore_backup(bt, bb).await;
    }
}

pub async fn transfer(
    parent: &adw::ApplicationWindow,
    sources: Vec<PathBuf>,
    folder: PathBuf,
    moving: bool,
    op: &Operation,
) -> Result<TransferReport, String> {
    let mut queue: VecDeque<(PathBuf, PathBuf, bool)> = VecDeque::new();
    let mut files_to_remove: Vec<PathBuf> = Vec::new();
    let mut cleanup: Vec<PathBuf> = Vec::new();
    let mut dir_perms: Vec<(PathBuf, fs::Permissions)> = Vec::new();
    let mut deferred_backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut created_paths: Vec<PathBuf> = Vec::new();
    let mut policy = ConflictPolicy::Ask;
    let mut skipped = 0usize;
    let mut cancelled = false;

    for source in sources {
        if let Some(name) = source.file_name() {
            queue.push_back((source.clone(), folder.join(name), true));
        }
    }

    while let Some((source, target, top_level)) = queue.pop_front() {
        if op.cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let s_meta = source.clone();
        let meta = match gio::spawn_blocking(move || fs::symlink_metadata(&s_meta))
            .await
            .map_err(|_| "Worker failed".to_owned())?
        {
            Ok(m) => m,
            Err(e) => {
                rollback_backups(&mut deferred_backups).await;
                return Err(e.to_string());
            }
        };
        let is_symlink = meta.file_type().is_symlink();
        let is_dir = !is_symlink && meta.is_dir();

        if moving {
            let s = source.clone();
            let t = target.clone();
            let same = gio::spawn_blocking(move || same_entry(&s, &t))
                .await
                .map_err(|_| "Worker failed".to_owned())?;
            if same {
                continue;
            }
        }
        if is_dir {
            let s = source.clone();
            let t = target.clone();
            if let Err(e) = gio::spawn_blocking(move || check_destination_sync(&s, &t))
                .await
                .map_err(|_| "Worker failed".to_owned())?
            {
                rollback_backups(&mut deferred_backups).await;
                return Err(e);
            }
        }

        let (target, backup) =
            match resolve_conflict(parent, &source, &target, is_dir, &mut policy).await {
                Ok(Some(res)) => res,
                Ok(None) => {
                    skipped += 1;
                    continue;
                }
                Err(e) if e == "Cancelled" => {
                    cancelled = true;
                    break;
                }
                Err(e) => {
                    rollback_backups(&mut deferred_backups).await;
                    return Err(e);
                }
            };

        if is_dir {
            let s = source.clone();
            let t = target.clone();
            if let Err(e) = gio::spawn_blocking(move || check_destination_sync(&s, &t))
                .await
                .map_err(|_| "Worker failed".to_owned())?
            {
                if let Some((bt, bb)) = backup {
                    restore_backup(bt, bb).await;
                }
                rollback_backups(&mut deferred_backups).await;
                return Err(e);
            }
        }

        let name = source
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        if moving {
            let s = source.clone();
            let t = target.clone();
            let rename_res = gio::spawn_blocking(move || fs::rename(&s, &t))
                .await
                .map_err(|_| "Worker failed".to_owned())?;
            match rename_res {
                Ok(()) => {
                    if let Some((_, bb)) = backup {
                        remove_backup(bb, op.cancel.clone()).await;
                    }
                    op.completed.set(op.completed.get() + 1);
                    op.progress(&name, 1, 1);
                    if top_level {
                        created_paths.push(target);
                    }
                    continue;
                }
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
                Err(e) => {
                    if let Some((bt, bb)) = backup {
                        restore_backup(bt, bb).await;
                    }
                    rollback_backups(&mut deferred_backups).await;
                    return Err(e.to_string());
                }
            }
        }

        if is_symlink {
            let s = source.clone();
            let t = target.clone();
            let res = gio::spawn_blocking(move || {
                let link = fs::read_link(&s)?;
                std::os::unix::fs::symlink(link, &t)?;
                Ok::<_, io::Error>(())
            })
            .await
            .map_err(|_| "Worker failed".to_owned())?;
            if let Err(e) = res {
                if let Some((bt, bb)) = backup {
                    restore_backup(bt, bb).await;
                }
                rollback_backups(&mut deferred_backups).await;
                return Err(e.to_string());
            }
            if moving {
                files_to_remove.push(source);
            }
            if let Some(pair) = backup {
                deferred_backups.push(pair);
            }
            op.completed.set(op.completed.get() + 1);
            op.progress(&name, 1, 1);
            if top_level {
                created_paths.push(target);
            }
        } else if is_dir {
            let s = source.clone();
            let t = target.clone();
            let perms = meta.permissions();
            let children = gio::spawn_blocking(move || {
                fs::create_dir(&t)?;
                let mut kids = Vec::new();
                for entry in fs::read_dir(&s)? {
                    let entry = entry?;
                    kids.push((entry.path(), t.join(entry.file_name())));
                }
                Ok::<_, io::Error>(kids)
            })
            .await
            .map_err(|_| "Worker failed".to_owned())?;
            let children = match children {
                Ok(c) => c,
                Err(e) => {
                    if let Some((bt, bb)) = backup {
                        restore_backup(bt, bb).await;
                    }
                    rollback_backups(&mut deferred_backups).await;
                    return Err(e.to_string());
                }
            };
            dir_perms.push((target.clone(), perms));
            if let Some(pair) = backup {
                deferred_backups.push(pair);
            }
            for (cs, ct) in children {
                queue.push_back((cs, ct, false));
            }
            if moving {
                cleanup.push(source);
            }
            op.completed.set(op.completed.get() + 1);
            op.progress(&name, 1, 1);
            if top_level {
                created_paths.push(target);
            }
        } else {
            if let Err(e) = copy_file_with_progress(&source, &target, &name, op).await {
                if let Some((bt, bb)) = backup {
                    restore_backup(bt, bb).await;
                }
                if e == "Operation cancelled" {
                    cancelled = true;
                    break;
                }
                rollback_backups(&mut deferred_backups).await;
                return Err(e);
            }
            if moving {
                files_to_remove.push(source);
            }
            if let Some(pair) = backup {
                deferred_backups.push(pair);
            }
            op.completed.set(op.completed.get() + 1);
            if top_level {
                created_paths.push(target);
            }
        }
    }

    let cancelled = cancelled || op.cancel.load(Ordering::Relaxed);
    if cancelled {
        rollback_backups(&mut deferred_backups).await;
    } else {
        for (dir, perms) in dir_perms.into_iter().rev() {
            let _ = gio::spawn_blocking(move || fs::set_permissions(&dir, perms)).await;
        }
        for file in files_to_remove {
            let _ = gio::spawn_blocking(move || fs::remove_file(&file)).await;
        }
        for dir in cleanup.into_iter().rev() {
            let _ = gio::spawn_blocking(move || fs::remove_dir(&dir)).await;
        }
        for (_, bb) in deferred_backups {
            remove_backup(bb, op.cancel.clone()).await;
        }
    }

    Ok(TransferReport {
        completed: op.completed.get(),
        skipped,
        cancelled,
        created_paths,
    })
}

async fn copy_file_with_progress(
    source: &Path,
    target: &Path,
    name: &str,
    op: &Operation,
) -> Result<(), String> {
    let s = source.to_path_buf();
    let t = target.to_path_buf();
    let cancel = op.cancel.clone();
    let (tx, rx) = std::sync::mpsc::channel::<(i64, i64)>();
    let op_clone = op.clone();
    let label_name = name.to_owned();
    let shared_rx = Rc::new(rx);
    let rx_timer = shared_rx.clone();
    let timer = glib::timeout_add_local(Duration::from_millis(60), move || {
        if let Some((cur, tot)) = rx_timer.try_iter().last() {
            op_clone.progress(&label_name, cur, tot);
        }
        glib::ControlFlow::Continue
    });

    let res = gio::spawn_blocking(move || {
        copy_file_sync(&s, &t, &cancel, |c, tot| {
            let _ = tx.send((c, tot));
        })
    })
    .await;
    timer.remove();
    if let Some((cur, tot)) = shared_rx.try_iter().last() {
        op.progress(name, cur, tot);
    }
    res.map_err(|_| "Copy worker failed".to_owned())?
}

fn copy_file_sync(
    source: &Path,
    target: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(i64, i64),
) -> Result<(), String> {
    let meta = fs::metadata(source).map_err(|e| e.to_string())?;
    if !meta.file_type().is_file() {
        return Err("Cannot copy special file (FIFO, socket, or device)".to_owned());
    }
    let total = meta.len() as i64;
    let mut reader = fs::File::open(source).map_err(|e| e.to_string())?;
    let mut writer = fs::File::create(target).map_err(|e| e.to_string())?;
    let copy_res = (|| -> Result<(), String> {
        let mut buf = vec![0u8; 256 * 1024];
        let mut copied = 0i64;
        let mut last = Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("Operation cancelled".to_owned());
            }
            let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            copied += n as i64;
            if last.elapsed() >= Duration::from_millis(40) {
                on_progress(copied, total);
                last = Instant::now();
            }
        }
        writer.flush().map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(e) = copy_res {
        drop(writer);
        let _ = fs::remove_file(target);
        return Err(e);
    }
    let _ = fs::set_permissions(target, meta.permissions());
    on_progress(total, total);
    Ok(())
}

pub fn duplicate_sync(
    sources: &[PathBuf],
    cancel: &AtomicBool,
) -> Result<Vec<PathBuf>, String> {
    let mut created = Vec::new();
    for src in sources {
        if cancel.load(Ordering::Relaxed) {
            return Err("Operation cancelled".to_owned());
        }
        let Some(parent) = src.parent() else { continue };
        let Some(name) = src.file_name().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let meta = fs::symlink_metadata(src).map_err(|e| e.to_string())?;
        let is_dir = !meta.file_type().is_symlink() && meta.is_dir();
        let dest = unique_copy_target(parent, &name, is_dir)?;
        if let Err(e) = copy_tree_sync(src, &dest, cancel) {
            let _ = remove_tree_sync(&dest, &AtomicBool::new(false));
            return Err(e);
        }
        created.push(dest);
    }
    Ok(created)
}

fn copy_tree_sync(from: &Path, to: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut dir_perms = Vec::new();
    copy_tree_sync_inner(from, to, cancel, &mut dir_perms)?;
    for (dir, perms) in dir_perms.into_iter().rev() {
        let _ = fs::set_permissions(&dir, perms);
    }
    Ok(())
}

fn copy_tree_sync_inner(
    from: &Path,
    to: &Path,
    cancel: &AtomicBool,
    dir_perms: &mut Vec<(PathBuf, fs::Permissions)>,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Operation cancelled".to_owned());
    }
    let meta = fs::symlink_metadata(from).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        let link = fs::read_link(from).map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(link, to).map_err(|e| e.to_string())?;
    } else if meta.is_dir() {
        fs::create_dir(to).map_err(|e| e.to_string())?;
        for entry in fs::read_dir(from).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_tree_sync_inner(&entry.path(), &to.join(entry.file_name()), cancel, dir_perms)?;
        }
        dir_perms.push((to.to_path_buf(), meta.permissions()));
    } else {
        copy_file_sync(from, to, cancel, |_, _| {})?;
    }
    Ok(())
}

pub fn remove_tree_sync(path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Operation cancelled".to_owned());
    }
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.file_type().is_symlink() && meta.is_dir() {
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            remove_tree_sync(&entry.path(), cancel)?;
        }
        fs::remove_dir(path).map_err(|e| e.to_string())
    } else {
        fs::remove_file(path).map_err(|e| e.to_string())
    }
}

pub async fn delete_paths(paths: Vec<PathBuf>, op: &Operation) -> Result<usize, String> {
    let cancel = op.cancel.clone();
    let total = paths.len() as i64;
    let mut done = 0usize;
    for path in paths {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        op.progress(&name, done as i64, total);
        let p = path.clone();
        let c = cancel.clone();
        gio::spawn_blocking(move || remove_tree_sync(&p, &c))
            .await
            .map_err(|_| "Delete worker failed".to_owned())??;
        done += 1;
        op.completed.set(done);
    }
    Ok(done)
}

pub async fn create_folder(dir: PathBuf, name: String) -> Result<PathBuf, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err("Invalid folder name".to_owned());
    }
    let target = dir.join(&name);
    let t = target.clone();
    gio::spawn_blocking(move || fs::create_dir(&t).map_err(|e| e.to_string()))
        .await
        .map_err(|_| "Folder worker failed".to_owned())??;
    Ok(target)
}

pub async fn create_file(dir: PathBuf, name: String) -> Result<PathBuf, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err("Invalid file name".to_owned());
    }
    let target = dir.join(&name);
    let t = target.clone();
    gio::spawn_blocking(move || {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&t)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "File worker failed".to_owned())??;
    Ok(target)
}

pub async fn rename_path(from: PathBuf, new_name: String) -> Result<PathBuf, String> {
    if new_name.is_empty() || new_name == "." || new_name == ".." || new_name.contains(['/', '\0']) {
        return Err("Invalid name".to_owned());
    }
    let Some(parent) = from.parent() else {
        return Err("Cannot rename root".to_owned());
    };
    let to = parent.join(&new_name);
    let f = from.clone();
    let t = to.clone();
    let p_dir = parent.to_path_buf();
    gio::spawn_blocking(move || {
        if let Ok(t_meta) = fs::symlink_metadata(&t) {
            let f_meta = fs::symlink_metadata(&f).map_err(|e| e.to_string())?;
            let same_inode = f_meta.dev() == t_meta.dev() && f_meta.ino() == t_meta.ino();
            let diff_name = f.file_name() != Some(OsStr::new(&new_name));
            if !(same_inode && diff_name) {
                return Err(format!("“{new_name}” already exists"));
            }
            let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp = p_dir.join(format!(".gavno-rename-{}-{seq}", std::process::id()));
            fs::rename(&f, &tmp).map_err(|e| e.to_string())?;
            if fs::symlink_metadata(&t).is_ok() {
                let _ = fs::rename(&tmp, &f);
                return Err(format!("“{new_name}” already exists"));
            }
            if let Err(e) = fs::rename(&tmp, &t) {
                let _ = fs::rename(&tmp, &f);
                return Err(e.to_string());
            }
            return Ok(());
        }
        fs::rename(&f, &t).map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "Rename worker failed".to_owned())??;
    Ok(to)
}

pub async fn chmod_path(path: PathBuf, mode: u32) -> Result<(), String> {
    gio::spawn_blocking(move || {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "Permission worker failed".to_owned())?
}

pub fn is_supported_archive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        ".zip", ".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst", ".tzst", ".tar.bz2",
        ".tbz2", ".7z", ".rar",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

fn archive_stem(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for ext in [
        ".tar.gz", ".tar.xz", ".tar.zst", ".tar.bz2", ".tgz", ".txz", ".tzst", ".tbz2", ".zip",
        ".tar", ".7z", ".rar",
    ] {
        if lower.ends_with(ext) && name.len() > ext.len() {
            return name[..name.len() - ext.len()].to_owned();
        }
    }
    format!("{name}_extracted")
}

struct StageGuard(Option<PathBuf>);

impl StageGuard {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        if let Some(ref path) = self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn run_command_with_cancel(
    mut cmd: Command,
    cancel: &AtomicBool,
    noun: &str,
) -> Result<(), String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Cannot run {noun}: {e}"))?;
    let stderr_handle = child.stderr.take().map(|mut se| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = io::Read::by_ref(&mut se).take(64 * 1024).read_to_end(&mut buf);
            let _ = io::copy(&mut se.take(4 * 1024 * 1024), &mut io::sink());
            String::from_utf8_lossy(&buf).trim().to_owned()
        })
    });
    let collect_err = |h: Option<std::thread::JoinHandle<String>>| {
        h.and_then(|t| t.join().ok()).unwrap_or_default()
    };

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = collect_err(stderr_handle);
            return Err(format!("{noun} cancelled"));
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let _ = collect_err(stderr_handle);
                return Ok(());
            }
            Ok(Some(status)) => {
                let err_text = collect_err(stderr_handle);
                return Err(if err_text.is_empty() {
                    format!("{noun} failed ({status})")
                } else {
                    err_text
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(30)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = collect_err(stderr_handle);
                return Err(e.to_string());
            }
        }
    }
}

fn common_ancestor(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut iter = paths.iter().filter_map(|p| p.parent());
    let first = iter.next()?;
    let mut comps: Vec<_> = first.components().collect();
    for parent in iter {
        let pcomps: Vec<_> = parent.components().collect();
        let shared = comps.iter().zip(&pcomps).take_while(|(a, b)| a == b).count();
        comps.truncate(shared);
    }
    if comps.is_empty() {
        return None;
    }
    Some(comps.iter().fold(PathBuf::new(), |mut acc, c| {
        acc.push(c.as_os_str());
        acc
    }))
}

pub async fn extract_archive(
    archive: PathBuf,
    dest_dir: PathBuf,
    op: &Operation,
) -> Result<PathBuf, String> {
    let cancel = op.cancel.clone();
    let name = archive
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_owned());
    op.progress(&name, 0, 0);
    gio::spawn_blocking(move || {
        let stem = archive_stem(&name);
        let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
        let stage = dest_dir.join(format!(".gavno-extract-{}-{seq}-{stem}", std::process::id()));
        fs::create_dir(&stage).map_err(|e| e.to_string())?;
        let guard = StageGuard(Some(stage.clone()));

        let archive_abs = if archive.is_absolute() {
            archive
        } else {
            std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(&archive)
        };
        let lower = name.to_ascii_lowercase();
        let cmd = if lower.ends_with(".zip") && command_in_path("unzip") {
            let mut c = Command::new("unzip");
            c.arg("-q").arg("-o").arg("-d").arg(&stage).arg(&archive_abs);
            c
        } else if command_in_path("bsdtar") {
            let mut c = Command::new("bsdtar");
            c.arg("-xf").arg(&archive_abs).arg("-C").arg(&stage);
            c
        } else {
            let mut c = Command::new("tar");
            c.arg("-xf").arg(&archive_abs).arg("-C").arg(&stage);
            c
        };

        run_command_with_cancel(cmd, &cancel, "Extraction")?;

        // Avoid tarbomb clutter: if the archive had a single top-level entry, hoist it cleanly.
        let mut items: Vec<_> = fs::read_dir(&stage)
            .map_err(|e| e.to_string())?
            .flatten()
            .collect();
        let final_target = if items.len() == 1 {
            let single = items.remove(0);
            let single_os = single.file_name();
            let single_lossy = single_os.to_string_lossy().into_owned();
            let is_dir = single.file_type().is_ok_and(|t| t.is_dir());
            let dest = if fs::symlink_metadata(dest_dir.join(&single_os)).is_err() {
                dest_dir.join(&single_os)
            } else {
                unique_copy_target(&dest_dir, &single_lossy, is_dir)?
            };
            fs::rename(single.path(), &dest).map_err(|e| e.to_string())?;
            drop(guard);
            dest
        } else {
            let dest = if fs::symlink_metadata(dest_dir.join(&stem)).is_err() {
                dest_dir.join(&stem)
            } else {
                unique_copy_target(&dest_dir, &stem, true)?
            };
            fs::rename(&stage, &dest).map_err(|e| e.to_string())?;
            guard.disarm();
            dest
        };
        Ok(final_target)
    })
    .await
    .map_err(|_| "Extract worker failed".to_owned())?
}

fn prepare_compress_args(
    sources: &[PathBuf],
    archive_path: &Path,
) -> Result<(PathBuf, PathBuf, Vec<std::ffi::OsString>), String> {
    if sources.is_empty() {
        return Err("No source files".to_owned());
    }
    let cwd = if sources.iter().all(|s| s.is_absolute()) {
        common_ancestor(sources).unwrap_or_else(|| PathBuf::from("/"))
    } else {
        common_ancestor(sources)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let mut rels = Vec::with_capacity(sources.len());
    for src in sources {
        let rel = if cwd == Path::new(".") {
            src.as_path()
        } else {
            src.strip_prefix(&cwd).map_err(|_| "Invalid source path".to_owned())?
        };
        // Prefix every item with "./" so filenames starting with '-' cannot act as CLI options.
        rels.push(Path::new(".").join(rel).into_os_string());
    }
    let abs_archive = if archive_path.is_absolute() {
        archive_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(archive_path)
    };
    Ok((cwd, abs_archive, rels))
}

pub async fn compress_paths(
    sources: Vec<PathBuf>,
    archive_path: PathBuf,
    op: &Operation,
) -> Result<PathBuf, String> {
    let cancel = op.cancel.clone();
    let name = archive_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_owned());
    op.progress(&name, 0, 0);
    gio::spawn_blocking(move || {
        let (cwd, abs_archive, names) = prepare_compress_args(&sources, &archive_path)?;
        let parent = abs_archive
            .parent()
            .ok_or_else(|| "Invalid archive path".to_owned())?;
        let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
        let lower = name.to_ascii_lowercase();
        let ext = if lower.ends_with(".zip") {
            ".zip"
        } else {
            ".tmp"
        };
        let staged = parent.join(format!(".gavno-compress-{}-{seq}{ext}", std::process::id()));
        let mut cmd = if lower.ends_with(".zip") && command_in_path("zip") {
            let mut c = Command::new("zip");
            c.arg("-r").arg("-q").arg(&staged).arg("--").args(&names);
            c
        } else if lower.ends_with(".tar.zst") || lower.ends_with(".tzst") {
            let mut c = Command::new("tar");
            c.arg("--zstd").arg("-cf").arg(&staged).arg("--").args(&names);
            c
        } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
            let mut c = Command::new("tar");
            c.arg("-cJf").arg(&staged).arg("--").args(&names);
            c
        } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") {
            let mut c = Command::new("tar");
            c.arg("-cjf").arg(&staged).arg("--").args(&names);
            c
        } else if lower.ends_with(".tar") {
            let mut c = Command::new("tar");
            c.arg("-cf").arg(&staged).arg("--").args(&names);
            c
        } else {
            let mut c = Command::new("tar");
            c.arg("-czf").arg(&staged).arg("--").args(&names);
            c
        };
        cmd.current_dir(cwd);

        if let Err(e) = run_command_with_cancel(cmd, &cancel, "Compression") {
            let _ = fs::remove_file(&staged);
            return Err(e);
        }
        if let Err(e) = fs::rename(&staged, &abs_archive) {
            let _ = fs::remove_file(&staged);
            return Err(e.to_string());
        }
        Ok(archive_path)
    })
    .await
    .map_err(|_| "Compression worker failed".to_owned())?
}

fn command_in_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|dir| {
            fs::metadata(dir.join(bin))
                .is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        })
    })
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

    #[test]
    fn copy_duplicate_and_remove_sync() {
        let dir = std::env::temp_dir().join(format!("gavno-ops-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        let file = dir.join("sub/note.txt");
        fs::write(&file, b"hello world").unwrap();
        let cancel = AtomicBool::new(false);

        let created = duplicate_sync(&[file.clone()], &cancel).unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0].file_name().unwrap().to_string_lossy(),
            "note (copy).txt"
        );
        assert_eq!(fs::read_to_string(&created[0]).unwrap(), "hello world");

        remove_tree_sync(&dir.join("sub"), &cancel).unwrap();
        assert!(!dir.join("sub").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_parent_replace_and_compress_guards() {
        let root = std::env::temp_dir().join(format!("gavno-ops-sec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let real_dir = root.join("real");
        let link_dir = root.join("link");
        fs::create_dir_all(&real_dir).unwrap();
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();
        let real_file = real_dir.join("a.txt");
        fs::write(&real_file, b"important").unwrap();

        assert!(is_self_or_ancestor_conflict(
            &real_file,
            &link_dir.join("a.txt")
        ));
        assert!(is_self_or_ancestor_conflict(&real_file, &real_dir));
        assert!(is_self_or_ancestor_conflict(
            &link_dir.join("a.txt"),
            &real_dir
        ));
        assert!(is_self_or_ancestor_conflict(
            &link_dir.join("a.txt"),
            &link_dir
        ));
        assert!(!is_self_or_ancestor_conflict(&real_file, &link_dir));
        assert!(check_destination_sync(&real_dir, &link_dir.join("sub")).is_err());

        // Cross-dir and flag-prefixed filenames stay safe with ./ prefix.
        let other = root.join("other/-weird");
        let (cwd, _, rels) =
            prepare_compress_args(&[real_file.clone(), other], &root.join("out.tar.gz")).unwrap();
        assert_eq!(cwd, root);
        assert_eq!(rels[0], "./real/a.txt");
        assert_eq!(rels[1], "./other/-weird");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rename_refuses_distinct_hardlink() {
        let root = std::env::temp_dir().join(format!("gavno-ops-hl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        fs::write(&a, b"data").unwrap();
        fs::hard_link(&a, &b).unwrap();

        let ctx = glib::MainContext::default();
        let res = ctx.block_on(rename_path(a.clone(), "b.txt".to_owned()));
        assert!(res.is_err());
        assert!(a.exists());
        assert!(b.exists());

        let _ = fs::remove_dir_all(&root);
    }
}
