//! Short-lived filesystem jobs and event-driven directory monitoring.
use super::{
    desktop::{Desktop, Item, Kind, Paths, Positions, Writer, MAX_ITEMS},
    widgets::Sample,
    ShellMode,
};
use crate::{module::Feature, Gavno};
use smithay::reexports::calloop::{
    channel,
    generic::Generic,
    timer::{TimeoutAction, Timer},
    Interest, Mode, PostAction,
};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

pub struct Scan {
    pub paths: Paths,
    pub items: Vec<Item>,
    pub positions: Option<Positions>,
}
pub enum Reply {
    Scan(io::Result<Scan>),
    Saved(io::Result<()>),
    Operation(io::Result<Option<(String, String)>>),
    /// A new file or folder: its name, so its rename starts once it shows.
    Created(io::Result<String>),
    /// "Open with" candidates for the item named here.
    OpenWith(String, super::desktop_menu::OpenWith),
    System(Sample),
}
pub fn paths() -> io::Result<Paths> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| io::Error::other("HOME must be an absolute directory"))?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let desktop = std::process::Command::new("xdg-user-dir")
        .arg("DESKTOP")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| p.is_absolute())
        .or_else(|| {
            fs::read_to_string(config.join("user-dirs.dirs"))
                .ok()
                .and_then(|s| desktop_from_dirs(&s, &home))
        })
        .unwrap_or_else(|| home.join("Desktop"));
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/state"))
        .join("gavno/desktop.json");
    Ok(Paths {
        desktop,
        state,
        home,
    })
}
pub fn desktop_from_dirs(text: &str, home: &Path) -> Option<PathBuf> {
    text.lines().find_map(|line| {
        let value = line
            .trim()
            .strip_prefix("XDG_DESKTOP_DIR=")?
            .trim()
            .strip_prefix('"')?
            .strip_suffix('"')?;
        let path = if value == "$HOME" {
            home.to_owned()
        } else if let Some(tail) = value.strip_prefix("$HOME/") {
            home.join(tail)
        } else {
            PathBuf::from(value)
        };
        path.is_absolute().then_some(path)
    })
}
pub fn icon_for(path: &Path, folder: bool) -> &'static str {
    if folder {
        return "folder";
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "svg" => "image-x-generic",
        "mp3" | "flac" | "ogg" | "wav" => "audio-x-generic",
        "mp4" | "mkv" | "webm" | "mov" => "video-x-generic",
        "pdf" => "application-pdf",
        "zip" | "gz" | "xz" | "zst" | "tar" | "7z" => "package-x-generic",
        "rs" | "c" | "h" | "py" | "js" | "toml" => "text-x-script",
        _ => "text-x-generic",
    }
}
fn small_read(path: &Path, max: u64) -> io::Result<String> {
    let mut text = String::new();
    fs::File::open(path)?.take(max).read_to_string(&mut text)?;
    Ok(text)
}
pub fn scan(paths: Paths, initial: bool, home: bool, trash: bool) -> io::Result<Scan> {
    fs::create_dir_all(&paths.desktop)?;
    let mut items = Vec::new();
    if home {
        items.push(Item {
            name: "\0home".into(),
            label: "Home".into(),
            icon: "user-home".into(),
            entry: None,
            kind: Kind::Home,
        });
    }
    if trash {
        items.push(Item {
            name: "\0trash".into(),
            label: "Trash".into(),
            icon: "user-trash".into(),
            entry: None,
            kind: Kind::Trash,
        });
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(&paths.desktop)? {
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        files.push((name, entry.path()));
        if files.len() >= MAX_ITEMS {
            break;
        }
    }
    files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    for (name, path) in files {
        let folder = path.is_dir();
        let entry = if path.extension().is_some_and(|e| e == "desktop") {
            small_read(&path, 64 * 1024)
                .ok()
                .and_then(|s| crate::desktop_entry::DesktopEntry::parse(&name, &s, Some(&path)))
        } else {
            None
        };
        let label: String = entry
            .as_ref()
            .map(|e| e.name.clone())
            .unwrap_or_else(|| name.clone())
            .chars()
            .take(128)
            .collect();
        let icon = entry
            .as_ref()
            .filter(|e| !e.icon.is_empty() && e.icon.len() <= 4096)
            .map(|e| e.icon.clone())
            .unwrap_or_else(|| icon_for(&path, folder).into());
        items.push(Item {
            name,
            label,
            icon,
            entry,
            kind: if folder { Kind::Folder } else { Kind::File },
        });
    }
    let positions = if initial {
        Some(
            small_read(&paths.state, 4 * 1024 * 1024)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
        )
    } else {
        None
    };
    Ok(Scan {
        paths,
        items,
        positions,
    })
}
pub fn atomic_write(
    path: &Path,
    positions: &Positions,
    writer: &Writer,
    sequence: u64,
) -> io::Result<()> {
    let _lock = writer.lock.lock().unwrap_or_else(|e| e.into_inner());
    if sequence != writer.sequence.load(Ordering::Acquire) {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("desktop state has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".desktop-{}-{sequence}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        serde_json::to_writer(&mut file, positions)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
pub fn rename(paths: &Paths, old: &str, new: &str) -> io::Result<()> {
    if !super::desktop::valid_name(new) {
        return Err(io::Error::other(
            "Name cannot be empty, contain /, or be . or ..",
        ));
    }
    let old =
        CString::new(paths.desktop.join(old).as_os_str().as_bytes()).map_err(io::Error::other)?;
    let new =
        CString::new(paths.desktop.join(new).as_os_str().as_bytes()).map_err(io::Error::other)?;
    // Linux's no-replace operation also closes the check/rename race.
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub fn new_folder(paths: &Paths) -> io::Result<String> {
    for n in 1..10000 {
        let name = if n == 1 {
            "New folder".into()
        } else {
            format!("New folder {n}")
        };
        match fs::create_dir(paths.desktop.join(&name)) {
            Ok(()) => return Ok(name),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("No free folder name"))
}

pub fn spawn_job(gv: &mut Gavno, job: impl FnOnce() -> Reply + Send + 'static) {
    let generation = gv.classic.desktop.generation;
    let (tx, rx) = channel::channel();
    match gv.loop_handle.insert_source(rx, move |event, _, gv| {
        let _stall = crate::perf::Stall::new("classic.desktop_reply");
        if let channel::Event::Msg(reply) = event {
            gv.guard(Feature::Classic, (), |gv| {
                if gv.classic.desktop.generation != generation || !gv.classic.desktop.active {
                    return;
                }
                gv.desktop_reply(reply);
            });
        }
    }) {
        Ok(_) => {
            std::thread::spawn(move || {
                let _ = tx.send(job());
            });
        }
        Err(error) => tracing::warn!(%error,"cannot register desktop job"),
    }
}
impl Gavno {
    pub fn refresh_classic_desktop(&mut self) {
        if !self.features.on(Feature::Classic) {
            return;
        }
        self.guard(Feature::Classic, (), |gv| {
            // Desktop icons live on the leftmost output that is in Classic.
            let loaded = gv.loaded_output();
            let classic_output = gv.outputs_in_order().into_iter().find(|o| {
                if loaded.is_none() || loaded.as_ref() == Some(o) {
                    gv.classic.mode == ShellMode::Classic || gv.shell_progress() > 0.001
                } else {
                    gv.shell_of(o).0 == ShellMode::Classic
                }
            });
            let needed = gv.greeter.is_none() && classic_output.is_some();
            if !needed {
                if gv.classic.desktop.active {
                    gv.stop_classic_desktop();
                    gv.classic.taskbar.release_caches();
                    gv.classic.start.release_caches();
                    crate::release_free_heap();
                }
                return;
            }
            // Headless harnesses opt in with isolated paths; never create a real
            // user's Desktop as a side effect of running the unit test suite.
            if matches!(gv.backend, crate::backend::Backend::None)
                && gv.classic.desktop.override_paths.is_none()
            {
                return;
            }
            let Some(output) = classic_output else {
                gv.stop_classic_desktop();
                return;
            };
            let Some(geo) = gv.space.output_geometry(&output) else {
                return;
            };
            let first = !gv.classic.desktop.active;
            if gv.classic.desktop.output.as_ref() != Some(&output) {
                gv.classic.desktop.layer = Default::default();
                gv.classic.desktop.overlay = Default::default();
                gv.classic.desktop.changed();
            }
            gv.classic.desktop.output = Some(output);
            let changed = gv.classic.desktop.set_bounds(geo.size);
            if first {
                gv.classic.desktop.active = true;
                gv.classic.desktop.generation = gv.classic.desktop.generation.wrapping_add(1);
                gv.classic
                    .desktop
                    .widgets
                    .configure(&gv.config.classic.widgets);
                gv.request_desktop_scan();
            } else if changed {
                gv.persist_desktop();
                gv.queue_redraw_all();
            }
            gv.reconcile_classic_widgets();
        });
    }
    pub fn stop_classic_desktop(&mut self) {
        for token in [
            self.classic.desktop.watch.take(),
            self.classic.desktop.debounce.take(),
            self.classic.desktop.widgets.clock_timer.take(),
            self.classic.desktop.widgets.system_timer.take(),
        ]
        .into_iter()
        .flatten()
        {
            self.loop_handle.remove(token);
        }
        let generation = self.classic.desktop.generation.wrapping_add(1);
        let writer = self.classic.desktop.writer.clone();
        let paths = self.classic.desktop.override_paths.clone();
        let mut desktop = Desktop::default();
        desktop.generation = generation;
        desktop.writer = writer;
        desktop.override_paths = paths;
        self.classic.desktop = desktop;
    }
    pub fn request_desktop_scan(&mut self) {
        if self.classic.desktop.loading {
            self.classic.desktop.rescan = true;
            return;
        }
        self.classic.desktop.loading = true;
        let initial = self.classic.desktop.paths.is_none();
        let known = self
            .classic
            .desktop
            .paths
            .clone()
            .or_else(|| self.classic.desktop.override_paths.clone());
        let home = self.config.classic.desktop_home;
        let trash = self.config.classic.desktop_trash;
        spawn_job(self, move || {
            Reply::Scan(
                known
                    .map(Ok)
                    .unwrap_or_else(paths)
                    .and_then(|p| scan(p, initial, home, trash)),
            )
        });
    }
    fn desktop_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Scan(result) => {
                self.classic.desktop.loading = false;
                match result {
                    Ok(scan) => {
                        let selected: BTreeSet<_> = self
                            .classic
                            .desktop
                            .selection
                            .items
                            .iter()
                            .filter_map(|i| {
                                self.classic
                                    .desktop
                                    .items
                                    .get(*i)
                                    .map(|item| item.name.clone())
                            })
                            .collect();
                        let anchor = self
                            .classic
                            .desktop
                            .selection
                            .anchor
                            .and_then(|i| self.classic.desktop.items.get(i))
                            .map(|item| item.name.clone());
                        let renamed = self
                            .classic
                            .desktop
                            .rename
                            .as_ref()
                            .and_then(|r| self.classic.desktop.items.get(r.index))
                            .map(|item| item.name.clone());
                        let menu_item = self
                            .classic
                            .desktop
                            .menu
                            .as_ref()
                            .and_then(|m| m.item)
                            .and_then(|i| self.classic.desktop.items.get(i))
                            .map(|item| item.name.clone());
                        let changed = self.classic.desktop.items != scan.items;
                        self.classic.desktop.paths = Some(scan.paths);
                        self.classic.desktop.items = scan.items;
                        if let Some(mut positions) = scan.positions {
                            positions.validate();
                            self.classic.desktop.positions = positions;
                        }
                        self.classic.desktop.selection.items = self
                            .classic
                            .desktop
                            .items
                            .iter()
                            .enumerate()
                            .filter_map(|(i, item)| selected.contains(&item.name).then_some(i))
                            .collect();
                        self.classic.desktop.selection.anchor = anchor.and_then(|name| {
                            self.classic
                                .desktop
                                .items
                                .iter()
                                .position(|item| item.name == name)
                        });
                        let rename_index = renamed.and_then(|name| {
                            self.classic
                                .desktop
                                .items
                                .iter()
                                .position(|item| item.name == name)
                        });
                        if let Some(index) = rename_index {
                            if let Some(rename) = &mut self.classic.desktop.rename {
                                rename.index = index;
                            }
                        } else {
                            self.classic.desktop.rename = None;
                        }
                        if let Some(name) = menu_item {
                            let index = self
                                .classic
                                .desktop
                                .items
                                .iter()
                                .position(|item| item.name == name);
                            if let Some(i) = index {
                                if let Some(menu) = &mut self.classic.desktop.menu {
                                    menu.item = Some(i);
                                }
                            } else {
                                self.classic.desktop.menu = None;
                            }
                        }
                        if changed {
                            self.classic.desktop.last_click = None;
                            if let Some(drag) = &self.classic.desktop.drag {
                                let cancel = match &drag.kind {
                                    super::desktop::DragKind::Rubber(_) => true,
                                    super::desktop::DragKind::Icons(group) => {
                                        group.keys().any(|name| {
                                            !self
                                                .classic
                                                .desktop
                                                .items
                                                .iter()
                                                .any(|item| &item.name == name)
                                        })
                                    }
                                    _ => false,
                                };
                                if cancel {
                                    self.classic.desktop.drag = None;
                                }
                            }
                        }
                        // A file or folder just made from the menu: select it
                        // and start renaming it, Windows style.
                        if let Some(name) = self.classic.desktop.pending_rename.take() {
                            match self.classic.desktop.items.iter().position(|item| item.name == name) {
                                Some(index) => {
                                    self.classic.desktop.selection.items = std::iter::once(index).collect();
                                    self.classic.desktop.selection.anchor = Some(index);
                                    self.start_desktop_rename(index);
                                }
                                None => self.classic.desktop.pending_rename = Some(name),
                            }
                        }
                        let repaired = self.classic.desktop.repair();
                        if repaired {
                            self.persist_desktop();
                        }
                        if changed || repaired {
                            self.classic.desktop.changed();
                            self.queue_redraw_all();
                        }
                        self.arm_desktop_watch();
                    }
                    Err(error) => tracing::warn!(%error,"cannot scan desktop directory"),
                }
                if std::mem::take(&mut self.classic.desktop.rescan) {
                    self.request_desktop_scan();
                }
            }
            Reply::Created(result) => match result {
                Ok(name) => {
                    self.classic.desktop.pending_rename = Some(name);
                    self.request_desktop_scan();
                }
                Err(error) => tracing::warn!(%error, "cannot create on the desktop"),
            },
            Reply::OpenWith(name, open_with) => {
                let d = &mut self.classic.desktop;
                let current = d.menu.as_ref().and_then(|m| m.item).and_then(|i| d.items.get(i)).map(|i| i.name.clone());
                if current.as_deref() == Some(name.as_str()) {
                    if let Some(menu) = &mut d.menu {
                        menu.open_with = open_with;
                    }
                    d.changed();
                    self.queue_redraw_all();
                }
            }
            Reply::Saved(result) => {
                if let Err(error) = result {
                    tracing::warn!(%error,"cannot save desktop positions");
                }
            }
            Reply::Operation(result) => {
                match result {
                    Ok(renamed) => {
                        if let Some((old, new)) = renamed {
                            if let Some(item) = self
                                .classic
                                .desktop
                                .items
                                .iter_mut()
                                .find(|item| item.name == old)
                            {
                                item.name = new.clone();
                                if item.entry.is_none() {
                                    item.label = new.clone();
                                }
                            }
                            if let Some(cell) = self.classic.desktop.positions.icons.remove(&old) {
                                self.classic.desktop.positions.icons.insert(new, cell);
                                self.persist_desktop();
                            }
                        }
                        self.classic.desktop.rename = None;
                        self.request_desktop_scan();
                    }
                    Err(error) => {
                        tracing::warn!(%error,"desktop operation failed");
                        if let Some(rename) = &mut self.classic.desktop.rename {
                            rename.error = Some(error.to_string());
                            rename.pending = false;
                        }
                        self.classic.desktop.changed();
                    }
                }
                self.classic.desktop.changed();
                self.queue_redraw_all();
            }
            Reply::System(sample) => {
                self.classic.desktop.widgets.receive(sample);
                if self.classic_widget_gate().active() {
                    self.queue_redraw_all();
                }
            }
        }
    }
    pub fn persist_desktop(&mut self) {
        let Some(paths) = &self.classic.desktop.paths else {
            return;
        };
        let path = paths.state.clone();
        let positions = self.classic.desktop.positions.clone();
        let writer = self.classic.desktop.writer.clone();
        let sequence = writer.sequence.fetch_add(1, Ordering::AcqRel) + 1;
        spawn_job(self, move || {
            Reply::Saved(atomic_write(&path, &positions, &writer, sequence))
        });
    }
    fn arm_desktop_watch(&mut self) {
        if self.classic.desktop.watch.is_some() {
            return;
        }
        // One verification scan closes the initial scan/watch installation gap.
        let Some(paths) = &self.classic.desktop.paths else {
            return;
        };
        let Ok(path) = CString::new(paths.desktop.as_os_str().as_bytes()) else {
            return;
        };
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            tracing::warn!(error=%io::Error::last_os_error(),"cannot create desktop watch");
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF;
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) } < 0 {
            tracing::warn!(error=%io::Error::last_os_error(),"cannot watch desktop directory");
            return;
        }
        match self.loop_handle.insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            |_, source, gv| {
                let _stall = crate::perf::Stall::new("classic.desktop_watch");
                let mut bytes = [0u8; 8192];
                let mut lost = false;
                loop {
                    let n = unsafe {
                        libc::read(source.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len())
                    };
                    if n > 0 {
                        let mut offset = 0;
                        while offset + std::mem::size_of::<libc::inotify_event>() <= n as usize {
                            let event = unsafe {
                                std::ptr::read_unaligned(
                                    bytes.as_ptr().add(offset).cast::<libc::inotify_event>(),
                                )
                            };
                            lost |= event.mask
                                & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED)
                                != 0;
                            offset +=
                                std::mem::size_of::<libc::inotify_event>() + event.len as usize;
                        }
                        continue;
                    }
                    if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                gv.guard(Feature::Classic, (), |gv| {
                    if lost {
                        gv.classic.desktop.watch = None;
                    }
                    gv.debounce_desktop_scan();
                });
                Ok(if lost {
                    PostAction::Remove
                } else {
                    PostAction::Continue
                })
            },
        ) {
            Ok(t) => {
                self.classic.desktop.watch = Some(t);
                self.classic.desktop.rescan = true;
            }
            Err(error) => tracing::warn!(%error,"cannot register desktop watch"),
        }
    }
    fn debounce_desktop_scan(&mut self) {
        if let Some(t) = self.classic.desktop.debounce.take() {
            self.loop_handle.remove(t);
        }
        if let Ok(t) = self.loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(100)),
            |_, _, gv| {
                let _stall = crate::perf::Stall::new("timer.desktop_debounce");
                gv.guard(Feature::Classic, (), |gv| {
                    gv.classic.desktop.debounce = None;
                    gv.request_desktop_scan();
                });
                TimeoutAction::Drop
            },
        ) {
            self.classic.desktop.debounce = Some(t);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::state::tests::Harness;
    use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
    pub(crate) fn setup() -> (Harness, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "gavno-desktop-{}-{}",
            std::process::id(),
            u32::from(smithay::utils::SERIAL_COUNTER.next_serial())
        ));
        fs::create_dir_all(root.join("Desktop")).unwrap();
        let mut h = Harness::new();
        let o = Output::new(
            "desktop-test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        o.change_current_state(
            Some(Mode {
                size: (1646, 926).into(),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );
        h.gv.space.map_output(&o, (0, 0));
        h.gv.config.classic.widgets.clear();
        h.gv.classic.desktop.override_paths = Some(Paths {
            desktop: root.join("Desktop"),
            state: root.join("state/desktop.json"),
            home: root.clone(),
        });
        h.gv.set_shell_mode(ShellMode::Classic);
        h.gv.classic.progress.jump(1.0);
        h.gv.settle_classic();
        h.gv.refresh_classic_desktop();
        (h, root)
    }
    pub(crate) fn pump(h: &mut Harness, condition: impl Fn(&Gavno) -> bool) {
        let end = std::time::Instant::now() + Duration::from_secs(4);
        while !condition(&h.gv) && std::time::Instant::now() < end {
            h.event_loop
                .dispatch(Duration::from_millis(10), &mut h.gv)
                .unwrap();
        }
        assert!(condition(&h.gv));
    }
    #[test]
    fn persistence_round_trip_new_folder_and_safe_rename() {
        let root = std::env::temp_dir().join(format!(
            "gavno-positions-{}",
            u32::from(smithay::utils::SERIAL_COUNTER.next_serial())
        ));
        fs::create_dir_all(&root).unwrap();
        let paths = Paths {
            desktop: root.clone(),
            state: root.join("state/desktop.json"),
            home: root.clone(),
        };
        let writer = Writer::default();
        writer.sequence.store(1, Ordering::Release);
        let mut positions = Positions::default();
        positions.icons.insert(
            "file.txt".into(),
            super::super::desktop::Cell { column: 2, row: 3 },
        );
        positions.widgets.insert("clock".into(), [300, 40]);
        atomic_write(&paths.state, &positions, &writer, 1).unwrap();
        assert_eq!(
            serde_json::from_str::<Positions>(&fs::read_to_string(&paths.state).unwrap()).unwrap(),
            positions
        );
        assert_eq!(new_folder(&paths).unwrap(), "New folder");
        assert_eq!(new_folder(&paths).unwrap(), "New folder 2");
        fs::write(root.join("old"), "old").unwrap();
        fs::write(root.join("existing"), "keep").unwrap();
        assert!(rename(&paths, "old", "existing").is_err());
        assert_eq!(fs::read_to_string(root.join("existing")).unwrap(), "keep");
        assert!(rename(&paths, "old", "bad/name").is_err());
        rename(&paths, "old", "renamed").unwrap();
        assert!(root.join("renamed").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn xdg_directory_expansion_and_desktop_file_metadata() {
        assert_eq!(
            desktop_from_dirs(
                "XDG_DESKTOP_DIR=\"$HOME/Рабочий стол\"",
                Path::new("/home/test")
            ),
            Some(PathBuf::from("/home/test/Рабочий стол"))
        );
        let (h, root) = setup();
        let paths = h.gv.classic.desktop.override_paths.clone().unwrap();
        fs::write(
            paths.desktop.join("app.desktop"),
            "[Desktop Entry]\nName=My App\nIcon=firefox\nExec=firefox %u\n",
        )
        .unwrap();
        fs::write(paths.desktop.join(".hidden"), "").unwrap();
        let scan = scan(paths, false, true, true).unwrap();
        assert_eq!(scan.items.len(), 3);
        let app = scan.items.iter().find(|i| i.name == "app.desktop").unwrap();
        assert_eq!(app.label, "My App");
        assert_eq!(app.icon, "firefox");
        drop(h);
        std::thread::sleep(Duration::from_millis(40));
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn inotify_debounces_and_rescans_through_calloop_channel() {
        let (mut h, root) = setup();
        pump(&mut h, |gv| {
            gv.classic.desktop.watch.is_some() && !gv.classic.desktop.loading
        });
        fs::write(root.join("Desktop/appeared.txt"), "hello").unwrap();
        pump(&mut h, |gv| {
            gv.classic
                .desktop
                .items
                .iter()
                .any(|i| i.name == "appeared.txt")
        });
        fs::remove_file(root.join("Desktop/appeared.txt")).unwrap();
        pump(&mut h, |gv| {
            !gv.classic
                .desktop
                .items
                .iter()
                .any(|i| i.name == "appeared.txt")
                && !gv.classic.desktop.loading
        });
        h.gv.set_shell_mode(ShellMode::Gavno);
        h.gv.classic.progress.jump(0.0);
        h.gv.settle_classic();
        h.gv.refresh_classic_desktop();
        assert!(h.gv.classic.desktop.watch.is_none());
        assert!(h.gv.classic.desktop.debounce.is_none());
        assert!(h.gv.classic.desktop.items.is_empty());
        std::thread::sleep(Duration::from_millis(30));
        let _ = fs::remove_dir_all(root);
    }
}

/// Creates `kind` on the desktop under the first free name, from `templates`
/// for a template, and returns that name.
pub fn create_entry(paths: &Paths, kind: &super::desktop_menu::NewKind, templates: Option<&Path>) -> io::Result<String> {
    use super::desktop_menu::NewKind;
    if *kind == NewKind::Folder {
        return new_folder(paths);
    }
    let base = kind.base_name();
    let (stem, ext) = split_name(&base);
    let source = match kind {
        NewKind::Template(name) => Some(fs::read(templates.ok_or_else(|| io::Error::other("no templates"))?.join(name))?),
        _ => None,
    };
    for n in 1..10000 {
        let name = if n == 1 { base.clone() } else { format!("{stem} {n}{ext}") };
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(paths.desktop.join(&name)) {
            Ok(mut file) => {
                file.write_all(source.as_deref().unwrap_or(kind.contents().as_bytes()))?;
                return Ok(name);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("No free file name"))
}

/// `report.tar.gz` -> (`report`, `.tar.gz`); a leading dot is part of the stem.
pub fn split_name(name: &str) -> (&str, &str) {
    let start = usize::from(name.starts_with('.'));
    match name[start..].find('.') {
        Some(i) => name.split_at(start + i),
        None => (name, ""),
    }
}

/// `~/Templates` (or the XDG templates directory) and the files in it.
pub fn templates() -> (Option<PathBuf>, Vec<String>) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = home.map(|h| h.join("Templates")).filter(|d| d.is_dir());
    let mut names: Vec<String> = dir.as_ref().and_then(|d| fs::read_dir(d).ok()).into_iter().flatten().flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names.truncate(16);
    (dir, names)
}

/// The MIME type of `path` from the shared-mime-info glob list, falling back
/// to text/plain for text and application/octet-stream otherwise.
pub fn mime_type(path: &Path) -> String {
    use std::sync::LazyLock;
    static GLOBS: LazyLock<(Vec<(String, String)>, Vec<(String, String)>)> = LazyLock::new(|| {
        let mut suffixes = Vec::new();
        let mut names = Vec::new();
        for dir in crate::desktop_entry::data_dirs() {
            let Ok(text) = fs::read_to_string(dir.join("mime/globs2")) else { continue };
            for line in text.lines().filter(|l| !l.starts_with('#')) {
                let mut parts = line.splitn(4, ':');
                let (Some(_weight), Some(mime), Some(glob)) = (parts.next(), parts.next(), parts.next()) else { continue };
                if let Some(suffix) = glob.strip_prefix('*') {
                    if !suffix.contains(['*', '?', '[']) {
                        suffixes.push((suffix.to_lowercase(), mime.to_owned()));
                    }
                } else if !glob.contains(['*', '?', '[']) {
                    names.push((glob.to_owned(), mime.to_owned()));
                }
            }
        }
        // Longest suffix first, so .tar.gz beats .gz; ties keep file order.
        suffixes.sort_by_key(|(s, _)| std::cmp::Reverse(s.len()));
        (suffixes, names)
    });
    if path.is_dir() {
        return "inode/directory".into();
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (suffixes, names) = &*GLOBS;
    if let Some((_, mime)) = names.iter().find(|(n, _)| *n == name) {
        return mime.clone();
    }
    let lower = name.to_lowercase();
    if let Some((_, mime)) = suffixes.iter().find(|(s, _)| lower.ends_with(s.as_str())) {
        return mime.clone();
    }
    let mut head = [0u8; 512];
    let read = fs::File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    if !head[..read].contains(&0) {
        "text/plain".into()
    } else {
        "application/octet-stream".into()
    }
}

fn mimeapps_default(mime: &str) -> Option<String> {
    let mut files = Vec::new();
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))) {
        files.push(config.join("mimeapps.list"));
    }
    files.push(PathBuf::from("/etc/xdg/mimeapps.list"));
    for dir in crate::desktop_entry::data_dirs() {
        files.push(dir.join("applications/mimeapps.list"));
    }
    for file in files {
        let Ok(text) = fs::read_to_string(file) else { continue };
        let mut section = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                section = line == "[Default Applications]";
                continue;
            }
            if let Some(value) = line.strip_prefix(mime).and_then(|r| r.trim_start().strip_prefix('=')).filter(|_| section) {
                if let Some(first) = value.split(';').map(str::trim).find(|v| !v.is_empty()) {
                    return Some(first.trim_end_matches(".desktop").to_owned());
                }
            }
        }
    }
    None
}

fn open_with_state() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|p| p.join("gavno/open-with"))
}

/// `mime<TAB>id;id;id` lines: the last apps used per type, newest first.
fn load_recent(path: &Path) -> Vec<(String, Vec<String>)> {
    fs::read_to_string(path).unwrap_or_default().lines().filter_map(|line| {
        let (mime, ids) = line.split_once('\t')?;
        Some((mime.to_owned(), ids.split(';').filter(|s| !s.is_empty()).map(str::to_owned).collect()))
    }).collect()
}

/// Puts `id` first among the last three apps used for `mime`.
pub fn remember_open_with(mime: &str, id: &str) {
    let Some(path) = open_with_state() else { return };
    let mut table = load_recent(&path);
    match table.iter_mut().find(|(m, _)| m == mime) {
        Some((_, ids)) => {
            ids.retain(|i| i != id);
            ids.insert(0, id.to_owned());
            ids.truncate(3);
        }
        None => table.push((mime.to_owned(), vec![id.to_owned()])),
    }
    table.truncate(256);
    let text: String = table.iter().map(|(m, ids)| format!("{m}\t{}\n", ids.join(";"))).collect();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, text).and_then(|_| fs::rename(&tmp, &path)).is_err() {
        tracing::warn!(path = %path.display(), "cannot save the Open with history");
    }
}

/// Apps for `path`: the last three used for its type first, else its default
/// app, then every other app that declares the type. Blocking, for workers.
pub fn open_with(path: &Path, theme: &str) -> super::desktop_menu::OpenWith {
    let mime = mime_type(path);
    let entries = crate::desktop_entry::load_all_with_icons(theme);
    let recent = open_with_state().map(|p| load_recent(&p)).unwrap_or_default()
        .into_iter().find(|(m, _)| *m == mime).map(|(_, ids)| ids).unwrap_or_default();
    rank_open_with(&mime, entries, &recent, mimeapps_default(&mime).as_deref())
}

pub fn rank_open_with(mime: &str, entries: Vec<crate::desktop_entry::DesktopEntry>, recent: &[String], default: Option<&str>) -> super::desktop_menu::OpenWith {
    let text = mime.starts_with("text/") || mime == "application/x-shellscript" || mime.ends_with("+xml") || mime == "application/json";
    let fits = |e: &crate::desktop_entry::DesktopEntry| {
        e.mime_types.iter().any(|m| m == mime)
            || (text && e.mime_types.iter().any(|m| m == "text/plain"))
            || (mime == "inode/directory" && e.categories.iter().any(|c| c == "TextEditor" || c == "IDE"))
    };
    let mut apps: Vec<_> = Vec::new();
    for id in recent {
        if let Some(e) = entries.iter().find(|e| &e.id == id) {
            apps.push(e.clone());
        }
    }
    let recent_count = apps.len();
    if let Some(e) = default.and_then(|d| entries.iter().find(|e| e.id == d)) {
        if !apps.contains(e) {
            apps.push(e.clone());
        }
    }
    let mut others: Vec<_> = entries.iter().filter(|e| fits(e) && !e.hidden_from_menus && !apps.contains(e)).cloned().collect();
    others.sort_by_key(|e| e.name.to_lowercase());
    apps.extend(others);
    apps.truncate(10);
    super::desktop_menu::OpenWith { mime: mime.to_owned(), apps, recent: recent_count, ready: true }
}

#[cfg(test)]
mod menu_io_tests {
    use super::*;
    use crate::desktop_entry::DesktopEntry;

    fn paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("gavno-menu-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Desktop")).unwrap();
        Paths { desktop: root.join("Desktop"), state: root.join("state"), home: root }
    }

    #[test]
    fn new_entries_get_free_names_contents_and_modes() {
        use super::super::desktop_menu::NewKind;
        let p = paths("new");
        assert_eq!(create_entry(&p, &NewKind::Text, None).unwrap(), "New text document.txt");
        assert_eq!(create_entry(&p, &NewKind::Text, None).unwrap(), "New text document 2.txt");
        assert_eq!(create_entry(&p, &NewKind::File, None).unwrap(), "New file");
        assert_eq!(create_entry(&p, &NewKind::File, None).unwrap(), "New file 2");
        assert!(fs::read(p.desktop.join("New file")).unwrap().is_empty());
        fs::create_dir_all(p.home.join("Templates")).unwrap();
        fs::write(p.home.join("Templates/Lab.cpp"), "// lab\n").unwrap();
        let made = create_entry(&p, &NewKind::Template("Lab.cpp".into()), Some(&p.home.join("Templates"))).unwrap();
        assert_eq!(fs::read_to_string(p.desktop.join(made)).unwrap(), "// lab\n");
        assert_eq!(create_entry(&p, &NewKind::Folder, None).unwrap(), "New folder");
        assert_eq!(split_name("a.tar.gz"), ("a", ".tar.gz"));
        assert_eq!(split_name(".bashrc"), (".bashrc", ""));
        let _ = fs::remove_dir_all(&p.home);
    }

    fn app(id: &str, mimes: &str) -> DesktopEntry {
        DesktopEntry::parse(id, &format!("[Desktop Entry]\nName={id}\nExec={id} %F\nMimeType={mimes}\n"), None).unwrap()
    }

    #[test]
    fn open_with_puts_recent_apps_first_then_the_default_then_the_rest() {
        let apps = vec![app("zathura", "application/pdf;"), app("evince", "application/pdf;"), app("code", "text/plain;"), app("mpv", "video/mp4;")];
        let fresh = rank_open_with("application/pdf", apps.clone(), &[], Some("evince"));
        assert_eq!(fresh.apps.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["evince", "zathura"]);
        assert_eq!(fresh.recent, 0);
        let used = rank_open_with("application/pdf", apps.clone(), &["zathura".into()], Some("evince"));
        assert_eq!(used.apps[0].id, "zathura");
        assert_eq!(used.recent, 1);
        // Text types also reach plain-text editors.
        let text = rank_open_with("text/x-c++src", apps, &[], None);
        assert_eq!(text.apps.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["code"]);
        let command = text.apps[0].command_with("foot", &["/home/me/a b.cpp".into()]).unwrap();
        assert_eq!(command, "'code' '/home/me/a b.cpp'");
    }
}
