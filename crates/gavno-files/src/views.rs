use crate::{
    clipboard,
    config::{SortField, ViewMode},
    icons::IconCache,
    model::FileEntry,
    thumbs::Thumbnails,
};
use gtk::{gdk, graphene, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
};

pub type Report = Rc<dyn Fn(String)>;
pub type RenameFn = Rc<dyn Fn(FileEntry, String)>;
pub type ContextMenuFn = Rc<dyn Fn(Option<u32>, f64, f64)>;
pub type MiddleClickFn = Rc<dyn Fn(u32)>;
pub type SortChangedFn = Rc<dyn Fn(SortField, bool)>;
pub type DropFn = Rc<dyn Fn(Option<PathBuf>, Vec<String>, bool, gdk::Drop)>;
pub type DragUrisFn = Rc<dyn Fn(u32) -> Vec<String>>;

pub struct ViewCallbacks {
    pub on_context_menu: ContextMenuFn,
    pub on_middle_click: MiddleClickFn,
    pub on_sort_changed: SortChangedFn,
    pub on_drop: DropFn,
    pub on_drag_uris: DragUrisFn,
    pub on_rename: RenameFn,
    pub report: Report,
}

pub struct Views {
    pub stack: gtk::Stack,
    pub store: gio::ListStore,
    pub selection: gtk::MultiSelection,
    pub grid: gtk::GridView,
    pub list: gtk::ColumnView,
    grid_scroll: gtk::ScrolledWindow,
    list_scroll: gtk::ScrolledWindow,
    active_mode: Rc<Cell<ViewMode>>,
    cut_uris: Rc<RefCell<HashSet<String>>>,
    bound_tiles: Rc<RefCell<HashMap<String, glib::WeakRef<gtk::Widget>>>>,
    sort_columns: Vec<(SortField, gtk::ColumnViewColumn)>,
    suppress_sort: Rc<Cell<bool>>,
    rename_popover: Rc<RefCell<Option<gtk::Popover>>>,
    scroll_source: Rc<RefCell<Option<glib::SourceId>>>,
    thumbs: Thumbnails,
    on_rename: RenameFn,
    report: Report,
}

impl Views {
    pub fn new(
        thumbs: Thumbnails,
        icon_cache: IconCache,
        callbacks: ViewCallbacks,
    ) -> Self {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::MultiSelection::new(Some(store.clone()));
        let cut_uris = Rc::new(RefCell::new(HashSet::new()));
        let bound_tiles = Rc::new(RefCell::new(HashMap::new()));

        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_hexpand(true);

        let grid_factory = build_grid_factory(
            icon_cache.clone(),
            thumbs.clone(),
            cut_uris.clone(),
            bound_tiles.clone(),
            stack.clone(),
            &callbacks,
        );
        let grid = gtk::GridView::new(Some(selection.clone()), Some(grid_factory));
        grid.set_min_columns(1);
        grid.set_max_columns(24);
        grid.set_single_click_activate(false);
        grid.set_enable_rubberband(true);
        grid.add_css_class("files-grid");

        let list = gtk::ColumnView::new(None::<gtk::SelectionModel>);
        list.set_single_click_activate(false);
        list.set_enable_rubberband(true);
        list.set_show_column_separators(false);
        list.set_show_row_separators(false);
        list.add_css_class("files-list");

        let mut sort_columns = Vec::new();
        let name_col = gtk::ColumnViewColumn::new(
            Some("Name"),
            Some(build_list_name_factory(
                icon_cache.clone(),
                thumbs.clone(),
                cut_uris.clone(),
                bound_tiles.clone(),
                stack.clone(),
                &callbacks,
            )),
        );
        name_col.set_expand(true);
        name_col.set_resizable(true);
        name_col.set_sorter(Some(&gtk::CustomSorter::new(|_, _| gtk::Ordering::Equal)));
        list.append_column(&name_col);
        sort_columns.push((SortField::Name, name_col));

        for (title, field_opt, width, kind) in [
            ("Size", Some(SortField::Size), 96, ListCellKind::Size),
            ("Modified", Some(SortField::Modified), 145, ListCellKind::Modified),
            ("Type", Some(SortField::Type), 160, ListCellKind::Type),
            ("Owner", None, 90, ListCellKind::Owner),
        ] {
            let col = gtk::ColumnViewColumn::new(
                Some(title),
                Some(build_list_text_factory(kind, stack.clone(), &callbacks)),
            );
            col.set_fixed_width(width);
            col.set_resizable(true);
            if let Some(sf) = field_opt {
                col.set_sorter(Some(&gtk::CustomSorter::new(|_, _| gtk::Ordering::Equal)));
                sort_columns.push((sf, col.clone()));
            }
            list.append_column(&col);
        }

        let suppress_sort = Rc::new(Cell::new(false));
        if let Some(sorter) = list.sorter().and_downcast::<gtk::ColumnViewSorter>() {
            let cols = sort_columns.clone();
            let suppress = suppress_sort.clone();
            let on_sort = callbacks.on_sort_changed.clone();
            sorter.connect_changed(move |s, _| {
                if suppress.get() {
                    return;
                }
                let Some(primary) = s.primary_sort_column() else {
                    on_sort(SortField::Name, false);
                    return;
                };
                if let Some((field, _)) = cols.iter().find(|(_, c)| *c == primary) {
                    let desc = s.primary_sort_order() == gtk::SortType::Descending;
                    on_sort(*field, desc);
                }
            });
        }

        let grid_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&grid)
            .build();
        let list_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();

        install_background_controllers(&grid, &stack, &callbacks, false);
        install_background_controllers(&list, &stack, &callbacks, true);

        stack.add_named(&grid_scroll, Some("grid"));
        stack.add_named(&list_scroll, Some("list"));
        stack.set_visible_child_name("grid");

        Self {
            stack,
            store,
            selection,
            grid,
            list,
            grid_scroll,
            list_scroll,
            active_mode: Rc::new(Cell::new(ViewMode::Grid)),
            cut_uris,
            bound_tiles,
            sort_columns,
            suppress_sort,
            rename_popover: Rc::new(RefCell::new(None)),
            scroll_source: Rc::new(RefCell::new(None)),
            thumbs,
            on_rename: callbacks.on_rename,
            report: callbacks.report,
        }
    }

    pub fn set_mode(&self, mode: ViewMode) {
        if self.active_mode.get() == mode {
            return;
        }
        self.active_mode.set(mode);
        self.bound_tiles.borrow_mut().clear();
        match mode {
            ViewMode::Grid => {
                self.list.set_model(None::<&gtk::SelectionModel>);
                self.grid.set_model(Some(&self.selection));
                self.stack.set_visible_child_name("grid");
            }
            ViewMode::List => {
                self.grid.set_model(None::<&gtk::SelectionModel>);
                self.list.set_model(Some(&self.selection));
                self.stack.set_visible_child_name("list");
            }
        }
    }

    pub fn mode(&self) -> ViewMode {
        self.active_mode.get()
    }

    pub fn set_sort_indicator(&self, field: SortField, descending: bool) {
        let Some((_, col)) = self.sort_columns.iter().find(|(f, _)| *f == field) else {
            return;
        };
        let order = if descending {
            gtk::SortType::Descending
        } else {
            gtk::SortType::Ascending
        };
        self.suppress_sort.set(true);
        self.list.sort_by_column(Some(col), order);
        self.suppress_sort.set(false);
    }

    pub fn grab_focus(&self) {
        match self.active_mode.get() {
            ViewMode::Grid => {
                self.grid.grab_focus();
            }
            ViewMode::List => {
                self.list.grab_focus();
            }
        }
    }

    pub fn scroll_value(&self) -> f64 {
        match self.active_mode.get() {
            ViewMode::Grid => self.grid_scroll.vadjustment().value(),
            ViewMode::List => self.list_scroll.vadjustment().value(),
        }
    }

    pub fn set_scroll_value(&self, value: f64) {
        if let Some(id) = self.scroll_source.borrow_mut().take() {
            id.remove();
        }
        let adj = match self.active_mode.get() {
            ViewMode::Grid => self.grid_scroll.vadjustment(),
            ViewMode::List => self.list_scroll.vadjustment(),
        };
        let slot = self.scroll_source.clone();
        let id = glib::idle_add_local_once(move || {
            slot.borrow_mut().take();
            let max = (adj.upper() - adj.page_size()).max(0.0);
            adj.set_value(value.clamp(0.0, max));
        });
        self.scroll_source.replace(Some(id));
    }

    pub fn replace_entries(&self, entries: &[FileEntry]) {
        self.thumbs.invalidate_pending();
        self.bound_tiles.borrow_mut().clear();
        let objects: Vec<glib::BoxedAnyObject> = entries
            .iter()
            .cloned()
            .map(glib::BoxedAnyObject::new)
            .collect();
        self.store.splice(0, self.store.n_items(), &objects);
    }

    pub fn append_entries(&self, entries: &[FileEntry]) {
        if entries.is_empty() {
            return;
        }
        let objects: Vec<glib::BoxedAnyObject> = entries
            .iter()
            .cloned()
            .map(glib::BoxedAnyObject::new)
            .collect();
        self.store.splice(self.store.n_items(), 0, &objects);
    }

    pub fn clear(&self) {
        self.thumbs.invalidate_pending();
        self.bound_tiles.borrow_mut().clear();
        self.store.remove_all();
    }

    pub fn n_items(&self) -> u32 {
        self.selection.n_items()
    }

    pub fn entry_at(&self, position: u32) -> Option<FileEntry> {
        self.selection
            .item(position)
            .and_downcast::<glib::BoxedAnyObject>()
            .and_then(|b| b.try_borrow::<FileEntry>().ok().map(|e| e.clone()))
    }

    pub fn selected_positions(&self) -> Vec<u32> {
        let set = self.selection.selection();
        let mut out = Vec::with_capacity(set.size() as usize);
        if let Some((iter, first)) = gtk::BitsetIter::init_first(&set) {
            out.push(first);
            out.extend(iter);
        }
        out
    }

    pub fn selected_count(&self) -> u64 {
        self.selection.selection().size()
    }

    pub fn selected(&self) -> Vec<FileEntry> {
        self.selected_positions()
            .into_iter()
            .filter_map(|pos| self.entry_at(pos))
            .collect()
    }

    pub fn selected_file_size_sum(&self) -> u64 {
        let set = self.selection.selection();
        let Some((iter, first)) = gtk::BitsetIter::init_first(&set) else {
            return 0;
        };
        let mut total = 0u64;
        for pos in std::iter::once(first).chain(iter) {
            if let Some(obj) = self
                .selection
                .item(pos)
                .and_downcast::<glib::BoxedAnyObject>()
            {
                if let Ok(e) = obj.try_borrow::<FileEntry>() {
                    if !e.is_dir {
                        total = total.saturating_add(e.size);
                    }
                }
            }
        }
        total
    }

    pub fn select_by_uris(&self, uris: &[String], scroll_to_first: bool) {
        if uris.is_empty() {
            return;
        }
        let target: HashSet<&str> = uris.iter().map(String::as_str).collect();
        let n = self.selection.n_items();
        let mut first_match = None;
        let bitset = gtk::Bitset::new_empty();
        for i in 0..n {
            if let Some(obj) = self
                .selection
                .item(i)
                .and_downcast::<glib::BoxedAnyObject>()
            {
                if let Ok(entry) = obj.try_borrow::<FileEntry>() {
                    if target.contains(entry.uri.as_str()) {
                        bitset.add(i);
                        if first_match.is_none() {
                            first_match = Some(i);
                        }
                    }
                }
            }
        }
        if !bitset.is_empty() {
            self.selection
                .set_selection(&bitset, &gtk::Bitset::new_range(0, n));
            if scroll_to_first {
                if let Some(pos) = first_match {
                    self.scroll_to(pos);
                }
            }
        }
    }

    pub fn scroll_to(&self, position: u32) {
        match self.active_mode.get() {
            ViewMode::Grid => {
                self.grid
                    .scroll_to(position, gtk::ListScrollFlags::NONE, None);
            }
            ViewMode::List => {
                self.list
                    .scroll_to(position, None, gtk::ListScrollFlags::NONE, None);
            }
        }
    }

    pub fn set_cut_uris(&self, uris: HashSet<String>) {
        self.cut_uris.replace(uris);
        let map = self.bound_tiles.borrow();
        let cut = self.cut_uris.borrow();
        for (uri, weak) in map.iter() {
            if let Some(widget) = weak.upgrade() {
                if cut.contains(uri) {
                    widget.add_css_class("cut-file");
                } else {
                    widget.remove_css_class("cut-file");
                }
            }
        }
    }

    pub fn dismiss_rename(&self) {
        let prev = self.rename_popover.borrow_mut().take();
        if let Some(prev) = prev {
            prev.popdown();
        }
    }

    pub fn open_rename_popover(&self, sibling_names: HashSet<String>) {
        let Some(pos) = self.selected_positions().into_iter().next() else {
            return;
        };
        let Some(entry) = self.entry_at(pos) else {
            return;
        };
        if entry.trash.is_some() {
            (self.report)("Restore files before renaming them".to_owned());
            return;
        }
        self.dismiss_rename();

        let popover = gtk::Popover::new();
        popover.add_css_class("rename-popover");
        popover.set_parent(&self.stack);
        let sw = self.stack.width() as f32;
        let sh = self.stack.height() as f32;
        let anchor_rect = self
            .bound_tiles
            .borrow()
            .get(&entry.uri)
            .and_then(|w| w.upgrade())
            .and_then(|tile| tile.compute_bounds(&self.stack))
            .filter(|b| {
                b.x() < sw && b.y() < sh && b.x() + b.width() > 0.0 && b.y() + b.height() > 0.0
            })
            .map(|bounds| {
                gdk::Rectangle::new(
                    bounds.x().round().max(0.0) as i32,
                    bounds.y().round().max(0.0) as i32,
                    bounds.width().round().max(1.0) as i32,
                    bounds.height().round().max(1.0) as i32,
                )
            });
        if let Some(rect) = anchor_rect {
            popover.set_pointing_to(Some(&rect));
        } else {
            let w = self.stack.width().max(200);
            popover.set_pointing_to(Some(&gdk::Rectangle::new(w / 2, 24, 1, 1)));
        }

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
        vbox.set_width_request(280);
        vbox.set_margin_top(6);
        vbox.set_margin_bottom(6);
        vbox.set_margin_start(6);
        vbox.set_margin_end(6);

        let title = gtk::Label::new(Some("Rename"));
        title.set_xalign(0.);
        title.add_css_class("heading");
        vbox.append(&title);

        let name_entry = gtk::Entry::new();
        name_entry.set_text(&entry.display_name);
        vbox.append(&name_entry);

        let err_label = gtk::Label::new(None);
        err_label.set_xalign(0.);
        err_label.add_css_class("error");
        err_label.add_css_class("caption");
        err_label.set_visible(false);
        vbox.append(&err_label);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.set_halign(gtk::Align::End);
        let cancel_btn = gtk::Button::with_label("Cancel");
        let save_btn = gtk::Button::with_label("Rename");
        save_btn.add_css_class("suggested-action");
        actions.append(&cancel_btn);
        actions.append(&save_btn);
        vbox.append(&actions);
        popover.set_child(Some(&vbox));

        let orig_name = entry.name.clone();
        let validate = Rc::new(move |text: &str| -> Result<(), &'static str> {
            if text.is_empty() || text == "." || text == ".." || text.contains(['/', '\0']) {
                return Err("Enter a valid name without '/'");
            }
            if text != orig_name && sibling_names.contains(text) {
                return Err("An item with this name already exists");
            }
            Ok(())
        });

        name_entry.connect_changed(glib::clone!(
            #[weak]
            err_label,
            #[weak]
            save_btn,
            #[strong]
            validate,
            move |e| {
                let text = e.text();
                match validate(&text) {
                    Ok(()) => {
                        err_label.set_visible(false);
                        save_btn.set_sensitive(true);
                    }
                    Err(msg) => {
                        err_label.set_text(msg);
                        err_label.set_visible(true);
                        save_btn.set_sensitive(false);
                    }
                }
            }
        ));

        let on_rename = self.on_rename.clone();
        let commit = Rc::new(glib::clone!(
            #[weak]
            popover,
            #[weak]
            name_entry,
            #[strong]
            entry,
            #[strong]
            on_rename,
            #[strong]
            validate,
            move || {
                let text = name_entry.text().to_string();
                if validate(&text).is_err() {
                    return;
                }
                popover.popdown();
                if text != entry.name {
                    on_rename(entry.clone(), text);
                }
            }
        ));

        save_btn.connect_clicked(glib::clone!(
            #[strong]
            commit,
            move |_| commit()
        ));
        name_entry.connect_activate(glib::clone!(
            #[strong]
            commit,
            move |_| commit()
        ));
        cancel_btn.connect_clicked(glib::clone!(
            #[weak]
            popover,
            move |_| popover.popdown()
        ));

        let grid_ref = self.grid.clone();
        let list_ref = self.list.clone();
        let mode_cell = self.active_mode.clone();
        let pop_slot = self.rename_popover.clone();
        popover.connect_closed(move |p| {
            let had_focus = p
                .root()
                .and_then(|r| r.focus())
                .is_none_or(|f| f == *p.upcast_ref::<gtk::Widget>() || f.is_ancestor(p));
            p.unparent();
            let mut guard = pop_slot.borrow_mut();
            if guard.as_ref() == Some(p) {
                guard.take();
            }
            drop(guard);
            if had_focus {
                match mode_cell.get() {
                    ViewMode::Grid => {
                        grid_ref.grab_focus();
                    }
                    ViewMode::List => {
                        list_ref.grab_focus();
                    }
                }
            }
        });
        self.rename_popover.replace(Some(popover.clone()));
        popover.popup();
        name_entry.grab_focus();

        let select_end = if !entry.is_dir && !entry.name.starts_with('.') {
            entry
                .display_name
                .rfind('.')
                .map(|b| entry.display_name[..b].chars().count() as i32)
                .unwrap_or(-1)
        } else {
            -1
        };
        name_entry.select_region(0, select_end);
    }
}

#[derive(Clone, Copy)]
enum ListCellKind {
    Size,
    Modified,
    Type,
    Owner,
}

fn entry_of(item: &gtk::ListItem) -> Option<FileEntry> {
    item.item()
        .and_downcast::<glib::BoxedAnyObject>()
        .and_then(|b| b.try_borrow::<FileEntry>().ok().map(|e| e.clone()))
}

fn unbind_tile_entry(
    item: &gtk::ListItem,
    bound_tiles: &Rc<RefCell<HashMap<String, glib::WeakRef<gtk::Widget>>>>,
) {
    if let (Some(entry), Some(w)) = (entry_of(item), item.child()) {
        let mut map = bound_tiles.borrow_mut();
        if map.get(&entry.uri).and_then(|wr| wr.upgrade()).as_ref() == Some(&w) {
            map.remove(&entry.uri);
        }
    }
}

fn attach_item_controllers(
    widget: &gtk::Widget,
    item: &gtk::ListItem,
    stack: &gtk::Stack,
    callbacks: &ViewCallbacks,
    allow_drag: bool,
) {
    // Right-click menu directly on the item (works across all columns in List view).
    let rclick = gtk::GestureClick::new();
    rclick.set_button(3);
    let on_menu = callbacks.on_context_menu.clone();
    rclick.connect_pressed(glib::clone!(
        #[weak]
        item,
        #[weak]
        widget,
        #[weak]
        stack,
        #[strong]
        on_menu,
        move |gesture, _, x, y| {
            let pos = item.position();
            if pos == gtk::INVALID_LIST_POSITION {
                return;
            }
            let (sx, sy) = widget
                .compute_point(&stack, &graphene::Point::new(x as f32, y as f32))
                .map(|p| (f64::from(p.x()), f64::from(p.y())))
                .unwrap_or((x, y));
            on_menu(Some(pos), sx, sy);
            gesture.set_state(gtk::EventSequenceState::Claimed);
        }
    ));
    widget.add_controller(rclick);

    // Middle-click opens folder in a new tab.
    let mclick = gtk::GestureClick::new();
    mclick.set_button(2);
    let on_middle = callbacks.on_middle_click.clone();
    mclick.connect_pressed(glib::clone!(
        #[weak]
        item,
        #[strong]
        on_middle,
        move |gesture, _, _, _| {
            let pos = item.position();
            if pos != gtk::INVALID_LIST_POSITION {
                on_middle(pos);
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
        }
    ));
    widget.add_controller(mclick);

    if allow_drag {
        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);
        let on_drag_uris = callbacks.on_drag_uris.clone();
        drag.connect_prepare(glib::clone!(
            #[weak]
            item,
            #[strong]
            on_drag_uris,
            #[upgrade_or]
            None,
            move |_, _, _| {
                let pos = item.position();
                if pos == gtk::INVALID_LIST_POSITION {
                    return None;
                }
                let uris = on_drag_uris(pos);
                if uris.is_empty() {
                    None
                } else {
                    Some(clipboard::Files { cut: false, uris }.provider())
                }
            }
        ));
        widget.add_controller(drag);
    }

    // Folder drop target; non-directories return false in `accept` so drops hit the current folder.
    let drop_target = gtk::DropTargetAsync::new(
        Some(gdk::ContentFormats::new(&[clipboard::URI_LIST])),
        gdk::DragAction::COPY | gdk::DragAction::MOVE,
    );
    drop_target.connect_accept(glib::clone!(
        #[weak]
        item,
        #[upgrade_or]
        false,
        move |_, _| entry_of(&item).is_some_and(|e| e.is_dir && e.trash.is_none())
    ));
    drop_target.connect_drag_enter(glib::clone!(
        #[weak]
        widget,
        #[upgrade_or]
        gdk::DragAction::empty(),
        move |controller, drop, _, _| {
            widget.add_css_class("drop-target");
            preferred_drop_action(controller.current_event_state(), drop)
        }
    ));
    drop_target.connect_drag_motion(move |controller, drop, _, _| {
        preferred_drop_action(controller.current_event_state(), drop)
    });
    drop_target.connect_drag_leave(glib::clone!(
        #[weak]
        widget,
        move |_, _| widget.remove_css_class("drop-target")
    ));
    let on_drop = callbacks.on_drop.clone();
    let report = callbacks.report.clone();
    drop_target.connect_drop(glib::clone!(
        #[weak]
        item,
        #[weak]
        widget,
        #[strong]
        on_drop,
        #[strong]
        report,
        #[upgrade_or]
        false,
        move |controller, drop, _, _| {
            widget.remove_css_class("drop-target");
            let Some(entry) = entry_of(&item) else {
                return false;
            };
            if !entry.is_dir || entry.trash.is_some() {
                return false;
            }
            let moving = is_move_drop(controller.current_event_state(), drop);
            let target_dir = entry.path;
            let drop = drop.clone();
            let on_drop = on_drop.clone();
            let report = report.clone();
            glib::spawn_future_local(async move {
                match clipboard::read_drop(&drop).await {
                    Ok(uris) => on_drop(Some(target_dir), uris, moving, drop),
                    Err(e) => {
                        report(e);
                        drop.finish(gdk::DragAction::empty());
                    }
                }
            });
            true
        }
    ));
    widget.add_controller(drop_target);
}

fn install_background_controllers(
    view: &impl IsA<gtk::Widget>,
    stack: &gtk::Stack,
    callbacks: &ViewCallbacks,
    skip_header: bool,
) {
    let view = view.as_ref();
    let rclick = gtk::GestureClick::new();
    rclick.set_button(3);
    let on_menu = callbacks.on_context_menu.clone();
    rclick.connect_pressed(glib::clone!(
        #[weak]
        view,
        #[weak]
        stack,
        #[strong]
        on_menu,
        move |gesture, _, x, y| {
            if skip_header {
                if let Some(picked) = view.pick(x, y, gtk::PickFlags::DEFAULT) {
                    if picked.ancestor(gtk::Button::static_type()).is_some() {
                        return;
                    }
                }
            }
            let (sx, sy) = view
                .compute_point(&stack, &graphene::Point::new(x as f32, y as f32))
                .map(|p| (f64::from(p.x()), f64::from(p.y())))
                .unwrap_or((x, y));
            on_menu(None, sx, sy);
            gesture.set_state(gtk::EventSequenceState::Claimed);
        }
    ));
    view.add_controller(rclick);

    let drop_target = gtk::DropTargetAsync::new(
        Some(gdk::ContentFormats::new(&[clipboard::URI_LIST])),
        gdk::DragAction::COPY | gdk::DragAction::MOVE,
    );
    drop_target.connect_drag_enter(move |controller, drop, _, _| {
        preferred_drop_action(controller.current_event_state(), drop)
    });
    drop_target.connect_drag_motion(move |controller, drop, _, _| {
        preferred_drop_action(controller.current_event_state(), drop)
    });
    let on_drop = callbacks.on_drop.clone();
    let report = callbacks.report.clone();
    drop_target.connect_drop(glib::clone!(
        #[strong]
        on_drop,
        #[strong]
        report,
        move |controller, drop, _, _| {
            let moving = is_move_drop(controller.current_event_state(), drop);
            let drop = drop.clone();
            let on_drop = on_drop.clone();
            let report = report.clone();
            glib::spawn_future_local(async move {
                match clipboard::read_drop(&drop).await {
                    Ok(uris) => on_drop(None, uris, moving, drop),
                    Err(e) => {
                        report(e);
                        drop.finish(gdk::DragAction::empty());
                    }
                }
            });
            true
        }
    ));
    view.add_controller(drop_target);
}

pub(crate) fn preferred_drop_action(
    state: gdk::ModifierType,
    drop: &gdk::Drop,
) -> gdk::DragAction {
    let actions = drop.actions();
    let pick = |pref: gdk::DragAction| {
        if actions.contains(pref) {
            pref
        } else if actions.contains(gdk::DragAction::COPY) {
            gdk::DragAction::COPY
        } else if actions.contains(gdk::DragAction::MOVE) {
            gdk::DragAction::MOVE
        } else {
            gdk::DragAction::empty()
        }
    };
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        pick(gdk::DragAction::MOVE)
    } else if state.contains(gdk::ModifierType::CONTROL_MASK) {
        pick(gdk::DragAction::COPY)
    } else if drop.drag().is_some() {
        pick(gdk::DragAction::MOVE)
    } else {
        pick(gdk::DragAction::COPY)
    }
}

pub(crate) fn is_move_drop(state: gdk::ModifierType, drop: &gdk::Drop) -> bool {
    let actions = drop.actions();
    if !actions.contains(gdk::DragAction::MOVE) {
        return false;
    }
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        return true;
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) && actions.contains(gdk::DragAction::COPY) {
        return false;
    }
    match drop.drag() {
        Some(d) => d.selected_action() == gdk::DragAction::MOVE,
        None => !actions.contains(gdk::DragAction::COPY),
    }
}

fn build_grid_factory(
    icon_cache: IconCache,
    thumbs: Thumbnails,
    cut_uris: Rc<RefCell<HashSet<String>>>,
    bound_tiles: Rc<RefCell<HashMap<String, glib::WeakRef<gtk::Widget>>>>,
    stack: gtk::Stack,
    callbacks: &ViewCallbacks,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let cbs_setup = (
        callbacks.on_context_menu.clone(),
        callbacks.on_middle_click.clone(),
        callbacks.on_sort_changed.clone(),
        callbacks.on_drop.clone(),
        callbacks.on_drag_uris.clone(),
        callbacks.on_rename.clone(),
        callbacks.report.clone(),
    );
    factory.connect_setup(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 6);
        tile.add_css_class("file-tile");
        tile.set_width_request(112);
        tile.set_height_request(110);
        tile.set_has_tooltip(true);
        tile.connect_query_tooltip(glib::clone!(
            #[weak]
            item,
            #[upgrade_or]
            false,
            move |_, _, _, _, tooltip| {
                let Some(entry) = entry_of(&item) else {
                    return false;
                };
                tooltip.set_text(Some(&format!(
                    "{}\n{} · {}",
                    entry.path.display(),
                    entry.kind_text,
                    entry.size_text
                )));
                true
            }
        ));

        let overlay = gtk::Overlay::new();
        overlay.set_halign(gtk::Align::Center);
        overlay.set_valign(gtk::Align::Center);
        let img = gtk::Image::new();
        img.set_pixel_size(56);
        img.add_css_class("thumb");
        overlay.set_child(Some(&img));

        let badge = gtk::Image::new();
        badge.set_pixel_size(14);
        badge.set_halign(gtk::Align::End);
        badge.set_valign(gtk::Align::End);
        badge.add_css_class("badge-icon");
        badge.set_visible(false);
        overlay.add_overlay(&badge);

        let name = gtk::Label::new(None);
        name.set_wrap(true);
        name.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        name.set_lines(2);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_justify(gtk::Justification::Center);
        name.set_max_width_chars(15);
        name.set_valign(gtk::Align::Start);

        let sub = gtk::Label::new(None);
        sub.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        sub.set_max_width_chars(16);
        sub.add_css_class("dim-label");
        sub.add_css_class("search-sub");
        sub.set_visible(false);

        tile.append(&overlay);
        tile.append(&name);
        tile.append(&sub);

        let cb = ViewCallbacks {
            on_context_menu: cbs_setup.0.clone(),
            on_middle_click: cbs_setup.1.clone(),
            on_sort_changed: cbs_setup.2.clone(),
            on_drop: cbs_setup.3.clone(),
            on_drag_uris: cbs_setup.4.clone(),
            on_rename: cbs_setup.5.clone(),
            report: cbs_setup.6.clone(),
        };
        attach_item_controllers(tile.upcast_ref(), item, &stack, &cb, true);
        item.set_child(Some(&tile));
    });

    let bound_bind = bound_tiles.clone();
    factory.connect_bind(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(entry) = entry_of(item) else {
            return;
        };
        let Some(tile) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        tile.remove_css_class("drop-target");
        let Some(overlay) = tile.first_child().and_downcast::<gtk::Overlay>() else {
            return;
        };
        let Some(img) = overlay.child().and_downcast::<gtk::Image>() else {
            return;
        };
        let Some(badge) = overlay.last_child().and_downcast::<gtk::Image>() else {
            return;
        };
        let Some(name_lbl) = overlay.next_sibling().and_downcast::<gtk::Label>() else {
            return;
        };
        let Some(sub_lbl) = tile.last_child().and_downcast::<gtk::Label>() else {
            return;
        };

        bound_bind
            .borrow_mut()
            .insert(entry.uri.clone(), tile.upcast_ref::<gtk::Widget>().downgrade());

        name_lbl.set_text(&entry.display_name);
        if entry.is_hidden {
            name_lbl.add_css_class("hidden-file");
        } else {
            name_lbl.remove_css_class("hidden-file");
        }
        if cut_uris.borrow().contains(&entry.uri) {
            tile.add_css_class("cut-file");
        } else {
            tile.remove_css_class("cut-file");
        }

        if let Some(ref sub) = entry.subtitle {
            sub_lbl.set_text(sub);
            sub_lbl.set_visible(true);
        } else {
            sub_lbl.set_visible(false);
        }

        if entry.is_broken_symlink {
            badge.set_icon_name(Some("dialog-warning-symbolic"));
            badge.set_visible(true);
        } else if entry.is_symlink {
            badge.set_icon_name(Some("emblem-symbolic-link-symbolic"));
            badge.set_visible(true);
        } else if entry.is_readonly && !entry.is_dir {
            badge.set_icon_name(Some("changes-prevent-symbolic"));
            badge.set_visible(true);
        } else {
            badge.set_visible(false);
        }

        let base_tex = icon_cache.texture(&entry.icon, 56);
        img.set_paintable(Some(&base_tex));

        if entry.is_thumbnail_supported {
            let uri = entry.uri.clone();
            let path = entry.path.clone();
            let mtime = entry.mtime;
            let size = entry.size;
            glib::spawn_future_local(glib::clone!(
                #[weak]
                item,
                #[weak]
                img,
                #[strong]
                thumbs,
                async move {
                    if let Some(tex) = thumbs.get(&path, &uri, mtime, size).await {
                        if entry_of(&item).is_some_and(|e| {
                            e.uri == uri
                                && e.is_thumbnail_supported
                                && e.mtime == mtime
                                && e.size == size
                        }) {
                            img.set_paintable(Some(&tex));
                        }
                    }
                }
            ));
        }
    });

    factory.connect_unbind(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        unbind_tile_entry(item, &bound_tiles);
        if let Some(tile) = item.child().and_downcast::<gtk::Box>() {
            tile.remove_css_class("drop-target");
            if let Some(overlay) = tile.first_child().and_downcast::<gtk::Overlay>() {
                if let Some(img) = overlay.child().and_downcast::<gtk::Image>() {
                    img.set_paintable(None::<&gdk::Paintable>);
                }
            }
        }
    });

    factory
}

fn build_list_name_factory(
    icon_cache: IconCache,
    thumbs: Thumbnails,
    cut_uris: Rc<RefCell<HashSet<String>>>,
    bound_tiles: Rc<RefCell<HashMap<String, glib::WeakRef<gtk::Widget>>>>,
    stack: gtk::Stack,
    callbacks: &ViewCallbacks,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let cbs_setup = (
        callbacks.on_context_menu.clone(),
        callbacks.on_middle_click.clone(),
        callbacks.on_sort_changed.clone(),
        callbacks.on_drop.clone(),
        callbacks.on_drag_uris.clone(),
        callbacks.on_rename.clone(),
        callbacks.report.clone(),
    );
    factory.connect_setup(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("file-tile");
        row.set_has_tooltip(true);
        row.connect_query_tooltip(glib::clone!(
            #[weak]
            item,
            #[upgrade_or]
            false,
            move |_, _, _, _, tooltip| {
                let Some(entry) = entry_of(&item) else {
                    return false;
                };
                tooltip.set_text(Some(&entry.path.to_string_lossy()));
                true
            }
        ));

        let img = gtk::Image::new();
        img.set_pixel_size(22);
        img.add_css_class("thumb");

        let labels = gtk::Box::new(gtk::Orientation::Vertical, 1);
        labels.set_hexpand(true);
        labels.set_valign(gtk::Align::Center);

        let name = gtk::Label::new(None);
        name.set_xalign(0.);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let sub = gtk::Label::new(None);
        sub.set_xalign(0.);
        sub.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        sub.add_css_class("dim-label");
        sub.add_css_class("search-sub");
        sub.set_visible(false);

        labels.append(&name);
        labels.append(&sub);

        let badge = gtk::Image::new();
        badge.set_pixel_size(13);
        badge.add_css_class("dim-label");
        badge.set_visible(false);

        row.append(&img);
        row.append(&labels);
        row.append(&badge);

        let cb = ViewCallbacks {
            on_context_menu: cbs_setup.0.clone(),
            on_middle_click: cbs_setup.1.clone(),
            on_sort_changed: cbs_setup.2.clone(),
            on_drop: cbs_setup.3.clone(),
            on_drag_uris: cbs_setup.4.clone(),
            on_rename: cbs_setup.5.clone(),
            report: cbs_setup.6.clone(),
        };
        attach_item_controllers(row.upcast_ref(), item, &stack, &cb, true);
        item.set_child(Some(&row));
    });

    let bound_bind = bound_tiles.clone();
    factory.connect_bind(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(entry) = entry_of(item) else {
            return;
        };
        let Some(row) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        row.remove_css_class("drop-target");
        let Some(img) = row.first_child().and_downcast::<gtk::Image>() else {
            return;
        };
        let Some(labels) = img.next_sibling().and_downcast::<gtk::Box>() else {
            return;
        };
        let Some(name_lbl) = labels.first_child().and_downcast::<gtk::Label>() else {
            return;
        };
        let Some(sub_lbl) = labels.last_child().and_downcast::<gtk::Label>() else {
            return;
        };
        let Some(badge) = row.last_child().and_downcast::<gtk::Image>() else {
            return;
        };

        bound_bind
            .borrow_mut()
            .insert(entry.uri.clone(), row.upcast_ref::<gtk::Widget>().downgrade());

        name_lbl.set_text(&entry.display_name);
        if entry.is_hidden {
            name_lbl.add_css_class("hidden-file");
        } else {
            name_lbl.remove_css_class("hidden-file");
        }
        if cut_uris.borrow().contains(&entry.uri) {
            row.add_css_class("cut-file");
        } else {
            row.remove_css_class("cut-file");
        }

        if let Some(ref sub) = entry.subtitle {
            sub_lbl.set_text(sub);
            sub_lbl.set_visible(true);
        } else {
            sub_lbl.set_visible(false);
        }

        if entry.is_broken_symlink {
            badge.set_icon_name(Some("dialog-warning-symbolic"));
            badge.set_visible(true);
        } else if entry.is_symlink {
            badge.set_icon_name(Some("emblem-symbolic-link-symbolic"));
            badge.set_visible(true);
        } else if entry.is_readonly && !entry.is_dir {
            badge.set_icon_name(Some("changes-prevent-symbolic"));
            badge.set_visible(true);
        } else {
            badge.set_visible(false);
        }

        let base_tex = icon_cache.texture(&entry.icon, 22);
        img.set_paintable(Some(&base_tex));

        if entry.is_thumbnail_supported {
            let uri = entry.uri.clone();
            let path = entry.path.clone();
            let mtime = entry.mtime;
            let size = entry.size;
            glib::spawn_future_local(glib::clone!(
                #[weak]
                item,
                #[weak]
                img,
                #[strong]
                thumbs,
                async move {
                    if let Some(tex) = thumbs.get(&path, &uri, mtime, size).await {
                        if entry_of(&item).is_some_and(|e| {
                            e.uri == uri
                                && e.is_thumbnail_supported
                                && e.mtime == mtime
                                && e.size == size
                        }) {
                            img.set_paintable(Some(&tex));
                        }
                    }
                }
            ));
        }
    });

    factory.connect_unbind(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        unbind_tile_entry(item, &bound_tiles);
        if let Some(row) = item.child().and_downcast::<gtk::Box>() {
            row.remove_css_class("drop-target");
            if let Some(img) = row.first_child().and_downcast::<gtk::Image>() {
                img.set_paintable(None::<&gdk::Paintable>);
            }
        }
    });

    factory
}

fn build_list_text_factory(
    kind: ListCellKind,
    stack: gtk::Stack,
    callbacks: &ViewCallbacks,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let cbs_setup = (
        callbacks.on_context_menu.clone(),
        callbacks.on_middle_click.clone(),
        callbacks.on_sort_changed.clone(),
        callbacks.on_drop.clone(),
        callbacks.on_drag_uris.clone(),
        callbacks.on_rename.clone(),
        callbacks.report.clone(),
    );
    factory.connect_setup(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = gtk::Label::new(None);
        label.set_xalign(0.);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.add_css_class("dim-label");
        let cb = ViewCallbacks {
            on_context_menu: cbs_setup.0.clone(),
            on_middle_click: cbs_setup.1.clone(),
            on_sort_changed: cbs_setup.2.clone(),
            on_drop: cbs_setup.3.clone(),
            on_drag_uris: cbs_setup.4.clone(),
            on_rename: cbs_setup.5.clone(),
            report: cbs_setup.6.clone(),
        };
        attach_item_controllers(label.upcast_ref(), item, &stack, &cb, false);
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(entry) = entry_of(item) else {
            return;
        };
        let Some(label) = item.child().and_downcast::<gtk::Label>() else {
            return;
        };
        label.remove_css_class("drop-target");
        label.set_text(match kind {
            ListCellKind::Size => &entry.size_text,
            ListCellKind::Modified => &entry.mtime_text,
            ListCellKind::Type => &entry.kind_text,
            ListCellKind::Owner => &entry.owner_text,
        });
    });
    factory.connect_unbind(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        if let Some(label) = item.child().and_downcast::<gtk::Label>() {
            label.remove_css_class("drop-target");
        }
    });
    factory
}
