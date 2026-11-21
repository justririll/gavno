//! Desktop input is claimed only after windows and shell surfaces decline it.
use super::{
    desktop::{Drag, DragKind, Kind, Menu, MenuAction, Rename, CELL_H, CELL_W},
    desktop_io::{self, Reply},
    ShellMode,
};
use crate::{module::Feature, Gavno};
use smithay::{
    input::keyboard::{keysyms, Keysym, ModifiersState},
    utils::{Logical, Point},
};
use std::process::Command;

impl Gavno {
    pub fn classic_desktop_local(&self, pos: Point<f64, Logical>) -> Option<Point<f64, Logical>> {
        if !self.features.on(Feature::Classic)
            || self.classic.settled != ShellMode::Classic
            || self.classic.mode != ShellMode::Classic
            || self.classic.progress.is_active()
            || !self.classic.desktop.active
            || self.protocols.locks.locked
            || self.greeter.is_some()
        {
            return None;
        }
        let output = self.classic.desktop.output.as_ref()?;
        let geo = self.space.output_geometry(output)?;
        geo.to_f64().contains(pos).then(|| pos - geo.loc.to_f64())
    }
    pub fn classic_desktop_available(&self, pos: Point<f64, Logical>) -> bool {
        if self.classic_desktop_local(pos).is_none()
            || self.shell_contains(pos)
            || self.desktop_transform_at(pos)
        {
            return false;
        }
        use smithay::wayland::shell::wlr_layer::Layer;
        if self
            .layer_hit(pos, &[Layer::Overlay, Layer::Top, Layer::Bottom])
            .is_some()
            || self.classic_decor_hit(pos).is_some()
            || self.window_input_at(pos).is_some()
            || self.resize_band_at(pos).is_some()
            || self.splitter_at(pos).is_some()
        {
            return false;
        }
        // An input-empty client surface still covers the icon visually.
        !self.space.elements().any(|w| {
            self.space.element_location(w).is_some_and(|loc| {
                smithay::utils::Rectangle::new(loc, w.geometry().size)
                    .to_f64()
                    .contains(pos)
            })
        })
    }
    pub fn classic_desktop_motion(&mut self, pos: Point<f64, Logical>) {
        if self.features.on(Feature::Classic) && self.classic.desktop.menu.is_some() {
            if let Some(p) = self.classic_desktop_local(pos) {
                let bounds = self.classic.desktop.menu_bounds();
                if self.classic.desktop.menu.as_mut().is_some_and(|m| m.hover(p, bounds)) {
                    self.classic.desktop.changed();
                    self.queue_redraw_all();
                }
            }
            return;
        }
        if !self.features.on(Feature::Classic) || self.classic.desktop.drag.is_none() {
            return;
        }
        self.guard(Feature::Classic, (), |gv| {
            let Some(geo) = gv
                .classic
                .desktop
                .output
                .as_ref()
                .and_then(|o| gv.space.output_geometry(o))
            else {
                return;
            };
            let p = pos - geo.loc.to_f64();
            let d = &mut gv.classic.desktop;
            let Some(mut drag) = d.drag.take() else {
                return;
            };
            drag.point = p;
            drag.moved |= (p.x - drag.origin.x).hypot(p.y - drag.origin.y) >= 4.0;
            if drag.moved {
                match &drag.kind {
                    DragKind::Rubber(initial) => {
                        let r = drag.rubber();
                        let rects: Vec<_> = (0..d.items.len())
                            .filter_map(|i| d.item_rect(i).map(|r| (i, r)))
                            .collect();
                        d.selection.rubber(r, rects.into_iter(), initial);
                    }
                    DragKind::Widget(name, origin) => {
                        let x = (origin[0] + (p.x - drag.origin.x).round() as i32)
                            .clamp(0, (d.size.w - 280).max(0));
                        let height = super::widgets::widget_height(name);
                        let y = (origin[1] + (p.y - drag.origin.y).round() as i32)
                            .clamp(0, (d.size.h - super::TASKBAR_HEIGHT - height).max(0));
                        d.positions.widgets.insert(name.clone(), [x, y]);
                        d.widgets.revision = d.widgets.revision.wrapping_add(1);
                    }
                    DragKind::Icons(_) => {}
                }
                d.changed();
                gv.queue_redraw_all();
            }
            gv.classic.desktop.drag = Some(drag);
        });
    }
    pub fn classic_desktop_button(&mut self, button: u32, pressed: bool) -> bool {
        if !self.features.on(Feature::Classic) {
            return false;
        }
        self.guard(Feature::Classic, false, |gv| {
            let pos = gv.pointer_location();
            // Releases must finish a desktop drag even above another window/output.
            if !pressed && button == 0x110 {
                if let Some(drag) = gv.classic.desktop.drag.take() {
                    if drag.moved {
                        match &drag.kind {
                            DragKind::Icons(group) => {
                                let dx =
                                    ((drag.point.x - drag.origin.x) / CELL_W as f64).round() as i32;
                                let dy =
                                    ((drag.point.y - drag.origin.y) / CELL_H as f64).round() as i32;
                                if gv.classic.desktop.move_group(group, dx, dy) {
                                    gv.persist_desktop();
                                }
                            }
                            DragKind::Widget(_, _) => {
                                gv.classic.desktop.repair();
                                gv.persist_desktop();
                            }
                            DragKind::Rubber(_) => {}
                        }
                    } else if matches!(&drag.kind, DragKind::Widget(name, _) if name == "music") {
                        if gv.classic_music_at(pos) {
                            gv.toggle_classic_music();
                        }
                    } else if let Some(i) = drag.clicked {
                        let ctrl = gv
                            .seat
                            .get_keyboard()
                            .is_some_and(|k| k.modifier_state().ctrl);
                        let shift = gv
                            .seat
                            .get_keyboard()
                            .is_some_and(|k| k.modifier_state().shift);
                        if !ctrl && !shift {
                            gv.classic.desktop.selection.click(i, false, false);
                        }
                    }
                    gv.classic.desktop.changed();
                    gv.queue_redraw_all();
                    return true;
                }
            }
            if gv.classic.desktop.menu.is_some() {
                if !pressed {
                    return true;
                }
                let bounds = gv.classic.desktop.menu_bounds();
                let local = gv.classic_desktop_local(pos);
                let clicked = match (local, gv.classic.desktop.menu.as_mut()) {
                    (Some(p), Some(menu)) if button == 0x110 => menu.click(p, bounds),
                    _ => Ok(None),
                };
                let Ok(action) = clicked else {
                    // A submenu row: it opens, the menu stays.
                    gv.classic.desktop.changed();
                    gv.queue_redraw_all();
                    return true;
                };
                let menu = gv.classic.desktop.menu.take();
                if let (Some(action), Some(menu)) = (action, menu) {
                    gv.desktop_action(action, &menu);
                }
                gv.classic.desktop.changed();
                gv.queue_redraw_all();
                return true;
            }
            let Some(p) = gv.classic_desktop_local(pos) else {
                return false;
            };
            if !gv.classic_desktop_available(pos) {
                return false;
            }
            if gv.seat.get_pointer().is_some_and(|p| p.is_grabbed()) {
                return false;
            }
            if !pressed {
                return true;
            }
            gv.focus_window(None);
            gv.classic.desktop.focused = true;
            if button == 0x111 {
                let item = gv.classic.desktop.hit(p);
                if let Some(i) = item {
                    if !gv.classic.desktop.selection.items.contains(&i) {
                        gv.classic.desktop.selection.click(i, false, false);
                    }
                }
                let info = item.and_then(|i| gv.classic.desktop.items.get(i).map(|it| (i, it.real(), it.kind == Kind::Folder)));
                let (_, templates) = desktop_io::templates();
                gv.classic.desktop.menu = Some(Menu::new(
                    p.to_i32_round(),
                    info,
                    super::desktop_menu::installed_editors(),
                    templates,
                ));
                // "Open with" needs the app list and the file type: a worker
                // fills it in while the menu is already showing.
                if let (Some((i, true, _)), Some(paths)) = (info, gv.classic.desktop.paths.clone()) {
                    let name = gv.classic.desktop.items[i].name.clone();
                    let theme = gv.config.dock.icon_theme.clone();
                    desktop_io::spawn_job(gv, move || {
                        let found = desktop_io::open_with(&paths.desktop.join(&name), &theme);
                        Reply::OpenWith(name, found)
                    });
                }
                gv.classic.desktop.changed();
                gv.queue_redraw_all();
                return true;
            }
            if button != 0x110 {
                return true;
            }
            gv.classic.desktop.rename = None;
            let widget = gv
                .classic
                .desktop
                .widgets
                .layouts(
                    &gv.classic.desktop.positions.widgets,
                    gv.classic.desktop.size,
                )
                .find(|(_, r)| r.to_f64().contains(p))
                .map(|(n, r)| (n.to_owned(), r));
            if let Some(command) = widget
                .as_ref()
                .filter(|(name, _)| name == "media" || name == "music")
                .and_then(|(name, r)| super::widgets::media_button(*r, p, name == "music"))
            {
                if widget.as_ref().is_some_and(|(name, _)| name == "music") {
                    gv.music_command(command);
                } else {
                    gv.media_command(command);
                }
                return true;
            }
            if let Some((name, origin)) = widget.map(|(n, r)| (n, [r.loc.x, r.loc.y])) {
                gv.classic.desktop.drag = Some(Drag {
                    origin: p,
                    point: p,
                    kind: DragKind::Widget(name, origin),
                    moved: false,
                    clicked: None,
                });
                return true;
            }
            let mods = gv
                .seat
                .get_keyboard()
                .map(|k| k.modifier_state())
                .unwrap_or_default();
            let hit = gv.classic.desktop.hit(p);
            let now = gv.start_time.elapsed();
            if let Some(index) = hit {
                if gv
                    .classic
                    .desktop
                    .last_click
                    .is_some_and(|(i, t)| i == index && now.saturating_sub(t).as_millis() < 400)
                {
                    gv.classic.desktop.last_click = None;
                    gv.open_desktop_item(index, false);
                    return true;
                }
                gv.classic.desktop.last_click = Some((index, now));
                if mods.ctrl || mods.shift || !gv.classic.desktop.selection.items.contains(&index) {
                    gv.classic
                        .desktop
                        .selection
                        .click(index, mods.ctrl, mods.shift);
                }
                let group = gv
                    .classic
                    .desktop
                    .selection
                    .items
                    .iter()
                    .filter_map(|i| {
                        let item = gv.classic.desktop.items.get(*i)?;
                        let cell = *gv.classic.desktop.positions.icons.get(&item.name)?;
                        Some((item.name.clone(), cell))
                    })
                    .collect();
                gv.classic.desktop.drag = Some(Drag {
                    origin: p,
                    point: p,
                    kind: DragKind::Icons(group),
                    moved: false,
                    clicked: Some(index),
                });
            } else {
                gv.classic.desktop.last_click = None;
                let initial = if mods.ctrl {
                    gv.classic.desktop.selection.items.clone()
                } else {
                    gv.classic.desktop.selection.items.clear();
                    Default::default()
                };
                gv.classic.desktop.drag = Some(Drag {
                    origin: p,
                    point: p,
                    kind: DragKind::Rubber(initial),
                    moved: false,
                    clicked: None,
                });
            }
            gv.classic.desktop.changed();
            gv.queue_redraw_all();
            true
        })
    }
    pub fn classic_desktop_key(
        &mut self,
        sym: Keysym,
        ch: Option<char>,
        mods: &ModifiersState,
    ) -> bool {
        if !self.features.on(Feature::Classic)
            || !self.classic.desktop.focused
            || self.focused_window().is_some()
            || self.classic.mode != ShellMode::Classic
            || self.protocols.locks.locked
            || self.greeter.is_some()
            || self.topbar.menu.is_some()
            || self.classic.start.open
            || self.launcher.open
            || mods.logo
            || mods.alt
        {
            return false;
        }
        self.guard(Feature::Classic, false, |gv| {
            if gv.classic.desktop.rename.is_some() {
                match sym.raw() {
                    keysyms::KEY_Escape => gv.classic.desktop.rename = None,
                    keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                        let Some(rename) = &mut gv.classic.desktop.rename else {
                            return true;
                        };
                        if rename.pending {
                            return true;
                        }
                        if !super::desktop::valid_name(&rename.value) {
                            rename.error = Some("Name cannot be empty or contain /".into());
                        } else if let (Some(paths), Some(item)) = (
                            gv.classic.desktop.paths.clone(),
                            gv.classic.desktop.items.get(rename.index),
                        ) {
                            let old = item.name.clone();
                            let new = rename.value.clone();
                            if old == new {
                                gv.classic.desktop.rename = None;
                            } else {
                                rename.pending = true;
                                desktop_io::spawn_job(gv, move || {
                                    Reply::Operation(
                                        desktop_io::rename(&paths, &old, &new)
                                            .map(|()| Some((old, new))),
                                    )
                                });
                            }
                        }
                    }
                    keysyms::KEY_BackSpace => {
                        if let Some(rename) = &mut gv.classic.desktop.rename {
                            rename.backspace(mods.ctrl);
                        }
                    }
                    keysyms::KEY_Delete => {
                        if let Some(rename) = &mut gv.classic.desktop.rename {
                            rename.delete();
                        }
                    }
                    keysyms::KEY_Left | keysyms::KEY_Right => {
                        if let Some(rename) = &mut gv.classic.desktop.rename {
                            rename.move_cursor(sym.raw() == keysyms::KEY_Right);
                        }
                    }
                    keysyms::KEY_Home | keysyms::KEY_End => {
                        if let Some(rename) = &mut gv.classic.desktop.rename {
                            rename.cursor = if sym.raw() == keysyms::KEY_Home {
                                0
                            } else {
                                rename.value.len()
                            };
                            rename.select_all = false;
                        }
                    }
                    keysyms::KEY_a | keysyms::KEY_A if mods.ctrl => {
                        if let Some(rename) = &mut gv.classic.desktop.rename {
                            rename.select_all = true;
                            rename.select_to = rename.value.len();
                            rename.cursor = rename.value.len();
                        }
                    }
                    _ => {
                        if let Some(c) = ch.filter(|c| !c.is_control() && !mods.ctrl) {
                            if let Some(rename) = &mut gv.classic.desktop.rename {
                                rename.insert(c);
                            }
                        }
                    }
                }
                gv.classic.desktop.changed();
                gv.queue_redraw_all();
                return true;
            }
            match sym.raw() {
                keysyms::KEY_Escape => {
                    gv.classic.desktop.menu = None;
                    gv.classic.desktop.selection.items.clear();
                }
                keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                    let items: Vec<_> =
                        gv.classic.desktop.selection.items.iter().copied().collect();
                    for i in items {
                        gv.open_desktop_item(i, false);
                    }
                }
                keysyms::KEY_Delete => gv.trash_desktop_selection(),
                keysyms::KEY_F5 => gv.request_desktop_scan(),
                keysyms::KEY_F2 => {
                    if let Some(i) = gv.classic.desktop.selection.items.iter().next().copied() {
                        gv.rename_desktop_item(i);
                    }
                }
                keysyms::KEY_a | keysyms::KEY_A if mods.ctrl => {
                    gv.classic.desktop.selection.items =
                        (0..gv.classic.desktop.items.len()).collect()
                }
                _ => return false,
            }
            gv.classic.desktop.changed();
            gv.queue_redraw_all();
            true
        })
    }
    fn rename_desktop_item(&mut self, index: usize) {
        self.start_desktop_rename(index);
    }
    /// Starts renaming with the whole name selected, extension included.
    pub(super) fn start_desktop_rename(&mut self, index: usize) {
        if let Some(item) = self
            .classic
            .desktop
            .items
            .get(index)
            .filter(|item| item.real())
        {
            let select_to = item.name.len();
            self.classic.desktop.rename = Some(Rename {
                index,
                value: item.name.clone(),
                cursor: select_to,
                select_all: true,
                select_to,
                pending: false,
                error: None,
            });
            self.classic.desktop.focused = true;
            self.classic.desktop.changed();
        }
    }
    fn open_desktop_item(&mut self, index: usize, reveal: bool) {
        let Some(item) = self.classic.desktop.items.get(index).cloned() else {
            return;
        };
        let Some(paths) = self.classic.desktop.paths.clone() else {
            return;
        };
        if reveal && item.kind != Kind::Trash {
            let path = if item.kind == Kind::Home {
                paths.home
            } else {
                paths.desktop.join(&item.name)
            };
            let file_manager = self.config.file_manager.clone();
            desktop_io::spawn_job(self, move || {
                let uri = file_uri(&path);
                let result = zbus::blocking::Connection::session().and_then(|c| {
                    c.call_method(
                        Some("org.freedesktop.FileManager1"),
                        "/org/freedesktop/FileManager1",
                        Some("org.freedesktop.FileManager1"),
                        "ShowItems",
                        &(vec![uri], String::new()),
                    )
                });
                if result.is_err() {
                    let parent = path.parent().unwrap_or(&path);
                    crate::state::spawn_shell(&format!(
                        "{file_manager} {}",
                        crate::desktop_entry::shell_quote(&parent.to_string_lossy())
                    ));
                }
                Reply::Saved(Ok(()))
            });
            return;
        }
        if let Some(entry) = item.entry {
            if let Some(command) = entry.command(&self.config.terminal) {
                crate::state::spawn_shell(&command);
            }
            return;
        }
        let target = match item.kind {
            Kind::Home => paths.home.to_string_lossy().into_owned(),
            Kind::Trash => "trash:///".into(),
            _ => paths
                .desktop
                .join(&item.name)
                .to_string_lossy()
                .into_owned(),
        };
        crate::state::spawn_shell(&format!(
            "xdg-open {}",
            crate::desktop_entry::shell_quote(&target)
        ));
    }
    fn trash_desktop_selection(&mut self) {
        let Some(paths) = &self.classic.desktop.paths else {
            return;
        };
        let files: Vec<_> = self
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
                    .filter(|item| item.real())
                    .map(|item| paths.desktop.join(&item.name))
            })
            .collect();
        if files.is_empty() {
            return;
        }
        desktop_io::spawn_job(self, move || {
            Reply::Operation(
                Command::new("gio")
                    .arg("trash")
                    .arg("--")
                    .args(files)
                    .status()
                    .and_then(|s| {
                        if s.success() {
                            Ok(None)
                        } else {
                            Err(std::io::Error::other("gio trash failed"))
                        }
                    }),
            )
        });
    }
    fn desktop_action(&mut self, action: MenuAction, menu: &Menu) {
        let item = menu.item;
        let target = || -> Option<std::path::PathBuf> {
            let paths = self.classic.desktop.paths.as_ref()?;
            Some(match item.and_then(|i| self.classic.desktop.items.get(i)) {
                Some(it) if it.real() => paths.desktop.join(&it.name),
                Some(_) => return None,
                None => paths.desktop.clone(),
            })
        };
        let target = target();
        match action {
            MenuAction::Open => {
                if let Some(i) = item {
                    self.open_desktop_item(i, false);
                }
            }
            MenuAction::Reveal => {
                if let Some(i) = item {
                    self.open_desktop_item(i, true);
                }
            }
            MenuAction::Rename => {
                if let Some(i) = item {
                    self.rename_desktop_item(i);
                }
            }
            MenuAction::Trash => self.trash_desktop_selection(),
            MenuAction::Refresh => self.request_desktop_scan(),
            MenuAction::Settings => {
                crate::state::spawn_shell(&self.config.classic.settings_command)
            }
            MenuAction::Files => {
                if let Some(paths) = &self.classic.desktop.paths {
                    crate::state::spawn_shell(&format!(
                        "{} {}",
                        self.config.file_manager,
                        crate::desktop_entry::shell_quote(&paths.desktop.to_string_lossy())
                    ));
                }
            }
            MenuAction::Terminal => {
                if let Some(dir) = &target {
                    crate::state::spawn_shell(&format!(
                        "cd -- {} && exec {}",
                        crate::desktop_entry::shell_quote(&dir.to_string_lossy()),
                        self.config.terminal
                    ));
                }
            }
            MenuAction::New(kind) => {
                if let Some(paths) = self.classic.desktop.paths.clone() {
                    let (templates, _) = desktop_io::templates();
                    desktop_io::spawn_job(self, move || {
                        Reply::Created(desktop_io::create_entry(&paths, &kind, templates.as_deref()))
                    });
                }
            }
            MenuAction::Editor(i) => {
                if let (Some(editor), Some(path)) = (menu.editors.get(i), &target) {
                    crate::state::spawn_shell(&format!(
                        "{} -- {}",
                        editor.command,
                        crate::desktop_entry::shell_quote(&path.to_string_lossy())
                    ));
                }
            }
            MenuAction::OpenWith(i) => {
                if let (Some(app), Some(path)) = (menu.open_with.apps.get(i).cloned(), target) {
                    let file = path.to_string_lossy().into_owned();
                    if let Some(command) = app.command_with(&self.config.terminal, &[file]) {
                        crate::state::spawn_shell(&command);
                        let mime = menu.open_with.mime.clone();
                        std::thread::spawn(move || desktop_io::remember_open_with(&mime, &app.id));
                    }
                }
            }
            MenuAction::CopyPath => {
                if let Some(path) = &target {
                    crate::state::spawn_shell(&format!(
                        "printf %s {} | wl-copy",
                        crate::desktop_entry::shell_quote(&path.to_string_lossy())
                    ));
                }
            }
        }
    }
    pub fn classic_desktop_axis(&mut self, vertical: f64) -> bool {
        let p = self.pointer_location();
        if !self.classic_desktop_available(p) {
            return false;
        }
        if vertical != 0.0 {
            let columns = self
                .classic
                .desktop
                .positions
                .icons
                .values()
                .map(|c| c.column + 1)
                .max()
                .unwrap_or(1);
            let pages = columns.div_ceil(self.classic.desktop.grid.columns);
            let d = &mut self.classic.desktop;
            d.page = if vertical > 0.0 {
                (d.page + 1).min(pages.saturating_sub(1))
            } else {
                d.page.saturating_sub(1)
            };
            d.changed();
            self.queue_redraw_all();
        }
        true
    }
}
fn file_uri(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            uri.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(uri, "%{b:02X}");
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_and_shell_surfaces_above_icons_take_the_click() {
        let (mut h, root) = super::super::desktop_io::tests::setup();
        super::super::desktop_io::tests::pump(&mut h, |gv| {
            !gv.classic.desktop.loading && gv.classic.desktop.watch.is_some()
        });
        assert!(h.gv.classic_desktop_available((30.0, 30.0).into()));
        let w = h.window();
        h.gv.map_new_window(w.clone());
        h.commit_size(&w, 400, 300);
        h.gv.space.map_element(w.clone(), (0, 0), false);
        assert!(!h.gv.classic_desktop_available((30.0, 30.0).into()));
        assert!(h.gv.classic_desktop_available((700.0, 30.0).into()));
        assert!(!h.gv.classic_desktop_available((700.0, 910.0).into()));
        h.gv.stop_classic_desktop();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn reveal_uri_preserves_spaces_and_non_ascii_bytes() {
        assert_eq!(
            file_uri(std::path::Path::new("/tmp/a b#c")),
            "file:///tmp/a%20b%23c"
        );
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    use smithay::input::pointer::MotionEvent;
    fn motion(gv: &mut Gavno, pos: Point<f64, Logical>) {
        let pointer = gv.seat.get_pointer().unwrap();
        pointer.motion(
            gv,
            None,
            &MotionEvent {
                location: pos,
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: 1,
            },
        );
    }
    #[test]
    fn music_widget_buttons_target_spotify_without_opening_panel() {
        let (mut h, root) = super::super::desktop_io::tests::setup();
        h.gv.classic.desktop.widgets.configure(&["music".into()]);
        let (tx, rx) = std::sync::mpsc::channel();
        h.gv.now_playing.borrow_mut().worker = Some(tx);
        let spotify = crate::media::Player {
            name: crate::media::MUSIC_PLAYER.into(),
            playback: crate::media::Playback::Playing,
            can_next: true,
            can_previous: true,
            can_pause: true,
            ..Default::default()
        };
        h.gv.media_update(crate::media::Snapshot {
            player: Some(crate::media::Player {
                name: "other".into(),
                ..Default::default()
            }),
            music_player: Some(spotify.clone()),
            music_available: true,
            ..Default::default()
        });
        let r =
            h.gv.classic
                .desktop
                .widgets
                .layouts(
                    &h.gv.classic.desktop.positions.widgets,
                    h.gv.classic.desktop.size,
                )
                .next()
                .unwrap()
                .1;
        h.gv.config.media.music_panel = false;
        let buttons: Vec<_> = crate::classic::widgets::media_buttons(r, true).collect();
        assert_eq!(buttons.len(), 3);
        for (command, b) in buttons {
            motion(
                &mut h.gv,
                (
                    b.loc.x as f64 + b.size.w as f64 / 2.0,
                    b.loc.y as f64 + b.size.h as f64 / 2.0,
                )
                    .into(),
            );
            assert!(h.gv.classic_desktop_button(0x110, true));
            assert!(h.gv.classic_desktop_button(0x110, false));
            assert!(
                matches!(rx.try_recv(), Ok(crate::media::mpris::Request::MusicCommand(c)) if c == command)
            );
            assert!(!h.gv.music_open());
            assert!(h.gv.classic.desktop.drag.is_none());
        }
        // Unsupported buttons remain consumed, without sending commands or opening.
        h.gv.media_update(crate::media::Snapshot {
            music_player: Some(crate::media::Player {
                can_next: false,
                ..spotify
            }),
            ..Default::default()
        });
        assert!(h.gv.classic_desktop_button(0x110, true));
        assert!(rx.try_recv().is_err());
        assert!(!h.gv.music_open());
        h.gv.stop_classic_desktop();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn f2_enter_renames_asynchronously_and_keeps_the_cell() {
        let (mut h, root) = super::super::desktop_io::tests::setup();
        std::fs::write(root.join("Desktop/old.txt"), "data").unwrap();
        super::super::desktop_io::tests::pump(&mut h, |gv| {
            !gv.classic.desktop.loading
                && gv.classic.desktop.items.iter().any(|i| i.name == "old.txt")
        });
        let index =
            h.gv.classic
                .desktop
                .items
                .iter()
                .position(|i| i.name == "old.txt")
                .unwrap();
        let cell = h.gv.classic.desktop.positions.icons["old.txt"];
        let r = h.gv.classic.desktop.item_rect(index).unwrap();
        motion(
            &mut h.gv,
            (r.loc.x as f64 + 30.0, r.loc.y as f64 + 30.0).into(),
        );
        assert!(h.gv.classic_desktop_button(0x110, true));
        assert!(h.gv.classic_desktop_button(0x110, false));
        let mods = ModifiersState::default();
        assert!(h
            .gv
            .classic_desktop_key(Keysym::from(keysyms::KEY_F2), None, &mods));
        for c in "bad/name".chars() {
            h.gv.classic_desktop_key(Keysym::from(0), Some(c), &mods);
        }
        h.gv.classic_desktop_key(Keysym::from(keysyms::KEY_Return), None, &mods);
        assert!(h
            .gv
            .classic
            .desktop
            .rename
            .as_ref()
            .unwrap()
            .error
            .is_some());
        assert!(root.join("Desktop/old.txt").exists());
        h.gv.classic_desktop_key(Keysym::from(keysyms::KEY_Escape), None, &mods);
        h.gv.classic_desktop_key(Keysym::from(keysyms::KEY_F2), None, &mods);
        for c in "renamed.txt".chars() {
            h.gv.classic_desktop_key(Keysym::from(0), Some(c), &mods);
        }
        h.gv.classic_desktop_key(Keysym::from(keysyms::KEY_Return), None, &mods);
        super::super::desktop_io::tests::pump(&mut h, |gv| {
            !gv.classic.desktop.loading
                && gv
                    .classic
                    .desktop
                    .items
                    .iter()
                    .any(|i| i.name == "renamed.txt")
        });
        assert!(!root.join("Desktop/old.txt").exists());
        assert_eq!(h.gv.classic.desktop.positions.icons["renamed.txt"], cell);
        h.gv.stop_classic_desktop();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn widget_drag_persists_and_desktop_menu_captures_outside_clicks() {
        let (mut h, root) = super::super::desktop_io::tests::setup();
        super::super::desktop_io::tests::pump(&mut h, |gv| {
            !gv.classic.desktop.loading && gv.classic.desktop.watch.is_some()
        });
        h.gv.classic.desktop.widgets.configure(&["clock".into()]);
        let origin =
            h.gv.classic
                .desktop
                .widgets
                .layouts(
                    &h.gv.classic.desktop.positions.widgets,
                    h.gv.classic.desktop.size,
                )
                .next()
                .unwrap()
                .1;
        let p = Point::from((origin.loc.x as f64 + 40.0, origin.loc.y as f64 + 30.0));
        motion(&mut h.gv, p);
        assert!(h.gv.classic_desktop_button(0x110, true));
        let to = p + Point::from((-200.0, 160.0));
        motion(&mut h.gv, to);
        h.gv.classic_desktop_motion(to);
        assert!(h.gv.classic_desktop_button(0x110, false));
        let target = h.gv.classic.desktop.positions.widgets["clock"];
        assert_eq!(target, [origin.loc.x - 200, origin.loc.y + 160]);
        super::super::desktop_io::tests::pump(&mut h, |_| {
            std::fs::read_to_string(root.join("state/desktop.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<super::super::desktop::Positions>(&s).ok())
                .is_some_and(|p| p.widgets.get("clock") == Some(&target))
        });
        motion(&mut h.gv, (600.0, 500.0).into());
        assert!(h.gv.classic_desktop_button(0x111, true));
        assert!(h.gv.classic.desktop.menu.is_some());
        assert!(h.gv.shell_contains((20.0, 20.0).into()));
        motion(&mut h.gv, (20.0, 20.0).into());
        assert!(h.gv.classic_desktop_button(0x110, true));
        assert!(h.gv.classic.desktop.menu.is_none());
        h.gv.stop_classic_desktop();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let _ = std::fs::remove_dir_all(root);
    }
}
