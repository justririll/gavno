//! Desktop model, collision-free grid, selection, and prepared rendering.
use super::{cache::Layer, widgets::Widgets, TASKBAR_HEIGHT};
use crate::{
    desktop_entry::DesktopEntry,
    render::GavnoRenderElement,
    topbar::geometry::{rect, Rect},
};
use serde::{Deserialize, Serialize};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    output::Output,
    reexports::calloop::RegistrationToken,
    utils::{Logical, Point, Size},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc, Mutex},
    time::Duration,
};

pub const CELL_W: i32 = 96;
pub const CELL_H: i32 = 104;
pub const MAX_ITEMS: usize = 4096;
pub const MAX_VISIBLE: usize = 256;
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cell {
    pub column: u32,
    pub row: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub columns: u32,
    pub rows: u32,
}
impl Grid {
    pub fn new(size: Size<i32, Logical>) -> Self {
        let rows = ((size.h - TASKBAR_HEIGHT - 16) / CELL_H).clamp(1, 64) as u32;
        Self {
            columns: ((size.w - 16) / CELL_W)
                .max(1)
                .min(MAX_VISIBLE as i32 / rows as i32) as u32,
            rows,
        }
    }
    pub fn at(self, index: usize) -> Cell {
        Cell {
            column: index as u32 / self.rows,
            row: index as u32 % self.rows,
        }
    }
    pub fn rect(self, cell: Cell, page: u32) -> Rect {
        rect(
            8 + (cell.column as i32 - page as i32 * self.columns as i32) * CELL_W,
            8 + cell.row as i32 * CELL_H,
            CELL_W,
            CELL_H,
        )
    }
    pub fn first_free(self, occupied: &BTreeSet<Cell>) -> Cell {
        (0..MAX_ITEMS * 2)
            .map(|i| self.at(i))
            .find(|c| !occupied.contains(c))
            .unwrap_or(Cell {
                column: MAX_ITEMS as u32,
                row: 0,
            })
    }
}
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Positions {
    pub version: u32,
    pub icons: BTreeMap<String, Cell>,
    pub widgets: BTreeMap<String, [i32; 2]>,
}
impl Positions {
    pub fn validate(&mut self) {
        self.version = 1;
        self.icons.retain(|name, c| {
            !name.contains('/')
                && name.len() <= 512
                && c.column < MAX_ITEMS as u32
                && c.row < MAX_ITEMS as u32
        });
        while self.icons.len() > MAX_ITEMS + 2 {
            self.icons.pop_last();
        }
        self.widgets
            .retain(|name, _| matches!(name.as_str(), "clock" | "system" | "media" | "music"));
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
    Home,
    Trash,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub label: String,
    pub icon: String,
    pub entry: Option<DesktopEntry>,
    pub kind: Kind,
}
impl Item {
    pub fn real(&self) -> bool {
        matches!(self.kind, Kind::File | Kind::Folder)
    }
}
#[derive(Default)]
pub struct Selection {
    pub items: BTreeSet<usize>,
    pub anchor: Option<usize>,
}
impl Selection {
    pub fn click(&mut self, index: usize, ctrl: bool, shift: bool) {
        if shift {
            let anchor = self.anchor.unwrap_or(index);
            if !ctrl {
                self.items.clear();
            }
            self.items.extend(anchor.min(index)..=anchor.max(index));
        } else {
            if !ctrl {
                self.items.clear();
            }
            if ctrl && !self.items.insert(index) {
                self.items.remove(&index);
            } else {
                self.items.insert(index);
            }
            self.anchor = Some(index);
        }
    }
    pub fn rubber(
        &mut self,
        bounds: Rect,
        rectangles: impl Iterator<Item = (usize, Rect)>,
        initial: &BTreeSet<usize>,
    ) {
        self.items = initial.clone();
        self.items
            .extend(rectangles.filter_map(|(i, r)| r.overlaps(bounds).then_some(i)));
    }
}
#[derive(Clone)]
pub enum DragKind {
    Rubber(BTreeSet<usize>),
    Icons(BTreeMap<String, Cell>),
    Widget(String, [i32; 2]),
}
pub struct Drag {
    pub origin: Point<f64, Logical>,
    pub point: Point<f64, Logical>,
    pub kind: DragKind,
    pub moved: bool,
    pub clicked: Option<usize>,
}
impl Drag {
    pub fn rubber(&self) -> Rect {
        rect(
            self.origin.x.min(self.point.x) as i32,
            self.origin.y.min(self.point.y) as i32,
            (self.point.x - self.origin.x).abs() as i32,
            (self.point.y - self.origin.y).abs() as i32,
        )
    }
}
pub struct Rename {
    pub index: usize,
    pub value: String,
    pub error: Option<String>,
    pub cursor: usize,
    pub select_all: bool,
    pub pending: bool,
}
impl Rename {
    fn clear_selection(&mut self) -> bool {
        if self.select_all {
            self.value.clear();
            self.cursor = 0;
            self.select_all = false;
            true
        } else {
            false
        }
    }
    pub fn insert(&mut self, c: char) {
        if self.pending {
            return;
        }
        self.clear_selection();
        if self.value.len() + c.len_utf8() <= 255 {
            self.value.insert(self.cursor, c);
            self.cursor += c.len_utf8();
            self.error = None;
        }
    }
    pub fn backspace(&mut self, all: bool) {
        if self.pending {
            return;
        }
        if all {
            self.value.clear();
            self.cursor = 0;
            self.select_all = false;
        } else if !self.clear_selection() && self.cursor > 0 {
            let previous = self.value[..self.cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i);
            self.value.replace_range(previous..self.cursor, "");
            self.cursor = previous;
        }
        self.error = None;
    }
    pub fn delete(&mut self) {
        if self.pending {
            return;
        }
        if !self.clear_selection() {
            if let Some(c) = self.value[self.cursor..].chars().next() {
                self.value
                    .replace_range(self.cursor..self.cursor + c.len_utf8(), "");
            }
        }
        self.error = None;
    }
    pub fn move_cursor(&mut self, right: bool) {
        if self.select_all {
            self.cursor = if right { self.value.len() } else { 0 };
            self.select_all = false;
        } else if right {
            self.cursor += self.value[self.cursor..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
        } else {
            self.cursor = self.value[..self.cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i);
        }
    }
}
pub struct Menu {
    pub point: Point<i32, Logical>,
    pub item: Option<usize>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Open,
    Reveal,
    Rename,
    Trash,
    Terminal,
    Files,
    NewFolder,
    Refresh,
    Settings,
}
impl Menu {
    pub fn actions(&self) -> &'static [(MenuAction, &'static str)] {
        if self.item.is_some() {
            &[
                (MenuAction::Open, "Open"),
                (MenuAction::Reveal, "Open in Files"),
                (MenuAction::Rename, "Rename"),
                (MenuAction::Trash, "Move to Trash"),
            ]
        } else {
            &[
                (MenuAction::Terminal, "Open terminal here"),
                (MenuAction::Files, "Open in Files"),
                (MenuAction::NewFolder, "New folder"),
                (MenuAction::Refresh, "Refresh"),
                (MenuAction::Settings, "Display settings"),
            ]
        }
    }
}
#[derive(Default)]
pub struct Writer {
    pub sequence: AtomicU64,
    pub lock: Mutex<()>,
}
#[derive(Clone)]
pub struct Paths {
    pub desktop: PathBuf,
    pub state: PathBuf,
    pub home: PathBuf,
}
pub struct Desktop {
    pub active: bool,
    pub generation: u64,
    pub loading: bool,
    pub rescan: bool,
    pub paths: Option<Paths>,
    pub override_paths: Option<Paths>,
    pub writer: Arc<Writer>,
    pub items: Vec<Item>,
    pub positions: Positions,
    pub selection: Selection,
    pub drag: Option<Drag>,
    pub rename: Option<Rename>,
    pub menu: Option<Menu>,
    pub focused: bool,
    pub output: Option<Output>,
    pub size: Size<i32, Logical>,
    pub grid: Grid,
    pub page: u32,
    pub watch: Option<RegistrationToken>,
    pub debounce: Option<RegistrationToken>,
    pub widgets: Widgets,
    pub layer: Layer,
    pub overlay: Layer,
    pub revision: u64,
    prepared: Option<(u64, u64, i32, i32, u64, [u32; 4])>,
    overlay_prepared: Option<(u64, u64)>,
    pub last_click: Option<(usize, Duration)>,
}
impl Default for Desktop {
    fn default() -> Self {
        Self {
            active: false,
            generation: 0,
            loading: false,
            rescan: false,
            paths: None,
            override_paths: None,
            writer: Arc::default(),
            items: Vec::new(),
            positions: Positions::default(),
            selection: Selection::default(),
            drag: None,
            rename: None,
            menu: None,
            focused: false,
            output: None,
            size: (0, 0).into(),
            grid: Grid {
                columns: 1,
                rows: 1,
            },
            page: 0,
            watch: None,
            debounce: None,
            widgets: Widgets::default(),
            layer: Layer::default(),
            overlay: Layer::default(),
            revision: 0,
            prepared: None,
            overlay_prepared: None,
            last_click: None,
        }
    }
}
impl Desktop {
    pub fn cancel_interaction(&mut self) {
        if let Some(Drag {
            kind: DragKind::Widget(name, origin),
            ..
        }) = self.drag.take()
        {
            self.positions.widgets.insert(name, origin);
        }
        self.menu = None;
        self.rename = None;
        self.focused = false;
        self.changed();
    }
    pub fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn repair(&mut self) -> bool {
        let names: BTreeSet<_> = self.items.iter().map(|item| item.name.as_str()).collect();
        let before = self.positions.icons.len();
        self.positions
            .icons
            .retain(|name, _| names.contains(name.as_str()));
        let mut changed = before != self.positions.icons.len();
        let grid = self.grid;
        let panels: Vec<_> = self
            .widgets
            .layouts(&self.positions.widgets, self.size)
            .map(|(_, r)| r)
            .collect();
        let blocked = |cell: Cell| {
            panels.iter().any(|r| {
                r.overlaps(grid.rect(
                    Cell {
                        column: cell.column % grid.columns,
                        row: cell.row,
                    },
                    0,
                ))
            })
        };
        let mut occupied = BTreeSet::new();
        let mut missing = Vec::new();
        for item in &self.items {
            if let Some(cell) = self
                .positions
                .icons
                .get(&item.name)
                .copied()
                .filter(|c| c.row < grid.rows && !blocked(*c))
            {
                if occupied.insert(cell) {
                    continue;
                }
            }
            missing.push(&item.name);
        }
        let mut cursor = 0;
        for name in missing {
            let cell = loop {
                let cell = grid.at(cursor);
                cursor += 1;
                if !occupied.contains(&cell) && !blocked(cell) {
                    break cell;
                }
                if cursor >= MAX_ITEMS * 4 {
                    break grid.at(MAX_ITEMS * 4 + occupied.len());
                }
            };
            occupied.insert(cell);
            self.positions.icons.insert(name.clone(), cell);
            changed = true;
        }
        changed
    }
    pub fn set_bounds(&mut self, size: Size<i32, Logical>) -> bool {
        if self.size == size {
            return false;
        }
        self.size = size;
        self.grid = Grid::new(size);
        self.changed();
        self.repair()
    }
    pub fn item_rect(&self, index: usize) -> Option<Rect> {
        let item = self.items.get(index)?;
        let cell = *self.positions.icons.get(&item.name)?;
        let mut r = self.grid.rect(cell, self.page);
        if let Some(Drag {
            kind: DragKind::Icons(selected),
            origin,
            point,
            moved: true,
            ..
        }) = &self.drag
        {
            if selected.contains_key(&item.name) {
                r.loc.x += (point.x - origin.x).round() as i32;
                r.loc.y += (point.y - origin.y).round() as i32;
            }
        }
        Some(r)
    }
    pub fn hit(&self, p: Point<f64, Logical>) -> Option<usize> {
        self.items.iter().enumerate().find_map(|(i, _)| {
            self.item_rect(i)
                .filter(|r| r.to_f64().contains(p))
                .map(|_| i)
        })
    }
    pub fn move_group(&mut self, group: &BTreeMap<String, Cell>, dx: i32, dy: i32) -> bool {
        if group.is_empty() {
            return false;
        }
        let occupied: BTreeSet<_> = self
            .positions
            .icons
            .iter()
            .filter(|(name, _)| !group.contains_key(*name))
            .map(|(_, c)| *c)
            .collect();
        let min_x = group.values().map(|c| c.column as i32).min().unwrap_or(0);
        let max_x = group.values().map(|c| c.column as i32).max().unwrap_or(0);
        let min_y = group.values().map(|c| c.row as i32).min().unwrap_or(0);
        let max_y = group.values().map(|c| c.row as i32).max().unwrap_or(0);
        let min_delta = (-min_x).max(dx - self.grid.columns as i32);
        let max_delta = (dx + self.grid.columns as i32)
            .min(MAX_ITEMS as i32 - 1 - max_x)
            .max(min_delta);
        let mut choices: Vec<_> = (min_delta..=max_delta)
            .flat_map(|x| {
                (-min_y..=(self.grid.rows as i32 - 1 - max_y).max(-min_y)).map(move |y| (x, y))
            })
            .collect();
        let panels: Vec<_> = self
            .widgets
            .layouts(&self.positions.widgets, self.size)
            .map(|(_, r)| r)
            .collect();
        choices.sort_by_key(|(x, y)| i64::from(x - dx).pow(2) + i64::from(y - dy).pow(2));
        let delta = choices.into_iter().find(|(x, y)| {
            group.values().all(|c| {
                !occupied.contains(&Cell {
                    column: (c.column as i32 + x) as u32,
                    row: (c.row as i32 + y) as u32,
                }) && !panels.iter().any(|r| {
                    r.overlaps(self.grid.rect(
                        Cell {
                            column: (c.column as i32 + x) as u32 % self.grid.columns,
                            row: (c.row as i32 + y) as u32,
                        },
                        0,
                    ))
                })
            })
        });
        let Some((dx, dy)) = delta else {
            return false;
        };
        if dx == 0 && dy == 0 {
            return false;
        }
        for (name, c) in group {
            self.positions.icons.insert(
                name.clone(),
                Cell {
                    column: (c.column as i32 + dx) as u32,
                    row: (c.row as i32 + dy) as u32,
                },
            );
        }
        self.changed();
        true
    }
    pub fn menu_rect(&self) -> Option<Rect> {
        let m = self.menu.as_ref()?;
        let height = m.actions().len() as i32 * 30 + 12;
        Some(rect(
            m.point.x.min((self.size.w - 244).max(0)),
            m.point
                .y
                .min((self.size.h - TASKBAR_HEIGHT - height - 8).max(0)),
            240,
            height,
        ))
    }
    pub fn menu_action(&self, p: Point<f64, Logical>) -> Option<MenuAction> {
        let r = self.menu_rect()?;
        if !r.to_f64().contains(p) {
            return None;
        }
        let i = ((p.y - r.loc.y as f64 - 6.0) / 30.0).floor() as isize;
        if i < 0 {
            return None;
        }
        self.menu
            .as_ref()?
            .actions()
            .get(i as usize)
            .map(|(action, _)| *action)
    }
    pub fn append(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        out: &mut Vec<GavnoRenderElement>,
        progress: f64,
        scale: f64,
        accent: [f32; 4],
        theme: &str,
    ) {
        if self.output.as_ref() != Some(output) || !self.active {
            return;
        }
        let key = (
            self.revision,
            scale.to_bits(),
            self.size.w,
            self.size.h,
            self.widgets.revision,
            accent.map(f32::to_bits),
        );
        if self.prepared != Some(key) {
            self.layer.begin(scale);
            let visible = rect(0, 0, self.size.w, self.size.h - TASKBAR_HEIGHT);
            for i in 0..self.items.len() {
                let Some(r) = self.item_rect(i).filter(|r| r.overlaps(visible)) else {
                    continue;
                };
                let item = &self.items[i];
                if self.selection.items.contains(&i) {
                    self.layer.panel(
                        rect(r.loc.x + 2, r.loc.y + 2, 92, 100),
                        6.0,
                        [accent[0], accent[1], accent[2], 0.28],
                        1.0,
                    );
                }
                self.layer
                    .icon(&item.icon, theme, rect(r.loc.x + 24, r.loc.y + 6, 48, 48));
                if let Some(rename) = self.rename.as_ref().filter(|rename| rename.index == i) {
                    self.layer.panel(
                        rect(r.loc.x, r.loc.y + 58, 96, 44),
                        3.0,
                        if rename.select_all {
                            [accent[0] * 0.4, accent[1] * 0.4, accent[2] * 0.4, 1.0]
                        } else {
                            [0.04, 0.05, 0.07, 1.0]
                        },
                        1.0,
                    );
                    self.layer.label(
                        &format!(
                            "{}▏{}",
                            &rename.value[..rename.cursor],
                            &rename.value[rename.cursor..]
                        ),
                        rect(r.loc.x + 3, r.loc.y + 60, 90, 40),
                        11.5,
                        2,
                        true,
                    );
                } else {
                    self.layer.label(
                        &item.label,
                        rect(r.loc.x + 3, r.loc.y + 60, 90, 40),
                        11.5,
                        2,
                        true,
                    );
                }
            }
            self.widgets
                .prepare(&mut self.layer, &self.positions.widgets, self.size, accent, theme);
            if let Some(drag) = &self.drag {
                if matches!(drag.kind, DragKind::Rubber(_)) && drag.moved {
                    self.layer.panel(
                        drag.rubber(),
                        1.0,
                        [accent[0], accent[1], accent[2], 0.18],
                        1.0,
                    );
                }
            }
            self.prepared = Some(key);
        }
        self.layer.append(
            renderer,
            out,
            progress,
            (self.size.w as f64 / 2.0, self.size.h as f64 / 2.0).into(),
        );
    }
    pub fn append_overlay(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        out: &mut Vec<GavnoRenderElement>,
        scale: f64,
    ) {
        if self.output.as_ref() != Some(output) {
            return;
        }
        let Some(panel) = self.menu_rect() else {
            return;
        };
        let key = (self.revision, scale.to_bits());
        if self.overlay_prepared != Some(key) {
            self.overlay.begin(scale);
            self.overlay
                .panel(panel, 8.0, [0.065, 0.075, 0.10, 0.98], 1.0);
            if let Some(menu) = &self.menu {
                for (i, (_, label)) in menu.actions().iter().enumerate() {
                    self.overlay.label(
                        label,
                        rect(panel.loc.x + 12, panel.loc.y + 13 + i as i32 * 30, 216, 20),
                        12.0,
                        1,
                        false,
                    );
                }
            }
            self.overlay_prepared = Some(key);
        }
        self.overlay.append(renderer, out, 1.0, (0.0, 0.0).into());
    }
}
pub fn valid_name(name: &str) -> bool {
    !name.trim().is_empty()
        && !matches!(name, "." | "..")
        && !name.contains('/')
        && !name.contains('\0')
        && name.len() <= 255
}

#[cfg(test)]
mod tests {
    use super::*;
    fn items(names: &[&str]) -> Vec<Item> {
        names
            .iter()
            .map(|name| Item {
                name: (*name).into(),
                label: (*name).into(),
                icon: "text-x-generic".into(),
                entry: None,
                kind: Kind::File,
            })
            .collect()
    }
    #[test]
    fn grid_is_column_major_and_repairs_collisions() {
        let mut d = Desktop::default();
        d.set_bounds((1280, 800).into());
        d.items = items(&["a", "b", "c", "d"]);
        d.positions
            .icons
            .insert("a".into(), Cell { column: 0, row: 0 });
        d.positions
            .icons
            .insert("b".into(), Cell { column: 0, row: 0 });
        d.repair();
        assert_eq!(d.positions.icons["a"], Cell { column: 0, row: 0 });
        assert_eq!(d.positions.icons["b"], Cell { column: 0, row: 1 });
        assert_eq!(d.grid.at(d.grid.rows as usize), Cell { column: 1, row: 0 });
        let cells: BTreeSet<_> = d.positions.icons.values().copied().collect();
        assert_eq!(cells.len(), 4);
        assert!(!d.repair());
        let group = BTreeMap::from([
            ("a".into(), d.positions.icons["a"]),
            ("b".into(), d.positions.icons["b"]),
        ]);
        assert!(d.move_group(&group, 1, 0));
        assert_eq!(d.positions.icons["a"], Cell { column: 1, row: 0 });
        assert_eq!(d.positions.icons["b"], Cell { column: 1, row: 1 });
        assert_eq!(
            d.positions
                .icons
                .values()
                .copied()
                .collect::<BTreeSet<_>>()
                .len(),
            4
        );
    }
    #[test]
    fn selection_click_ctrl_shift_and_rubber() {
        let mut s = Selection::default();
        s.click(2, false, false);
        assert_eq!(s.items, BTreeSet::from([2]));
        s.click(4, true, false);
        assert_eq!(s.items, BTreeSet::from([2, 4]));
        s.click(2, true, false);
        assert_eq!(s.items, BTreeSet::from([4]));
        s.click(4, false, false);
        s.click(1, false, true);
        assert_eq!(s.items, BTreeSet::from([1, 2, 3, 4]));
        s.rubber(
            rect(0, 0, 100, 110),
            [(0, rect(8, 8, 96, 104)), (1, rect(104, 8, 96, 104))].into_iter(),
            &BTreeSet::new(),
        );
        assert_eq!(s.items, BTreeSet::from([0]));
    }
    #[test]
    fn rename_validation_rejects_paths_and_empty_names() {
        for name in ["", " ", ".", "..", "a/b", "a\0b"] {
            assert!(!valid_name(name));
        }
        for name in ["report.txt", "Новая папка", ".hidden", "file with spaces"] {
            assert!(valid_name(name));
        }
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use smithay::backend::{
        allocator::Fourcc,
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::{
            damage::OutputDamageTracker,
            gles::{GlesRenderer, GlesTexture},
            Bind, Offscreen, Renderer,
        },
    };
    #[test]
    fn rss_and_static_desktop_cache_has_no_frame_allocations_or_layout() {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let mut h = crate::state::tests::Harness::new();
        let output = Output::new(
            "desktop-rss".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: (1646, 926).into(),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );
        h.gv.space.map_output(&output, (0, 0));
        let mut warm = crate::topbar::Canvas::default();
        warm.begin(&mut renderer, 1.75);
        warm.rect(
            rect(0, 0, 100, 50),
            6.0,
            [0.1, 0.1, 0.1, 1.0],
            1.0,
            crate::topbar::LINE,
        );
        warm.label(&mut renderer, "Normal shell", 8, 8, 80, 12.0);
        let warm_elements = warm.finish();
        let mut target: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, (2880, 1620).into())
            .unwrap();
        let mut tracker =
            OutputDamageTracker::new((2880, 1620), 1.75, smithay::utils::Transform::Normal);
        {
            let mut frame = renderer.bind(&mut target).unwrap();
            tracker
                .render_output(
                    &mut renderer,
                    &mut frame,
                    0,
                    &warm_elements,
                    crate::render::CLEAR_COLOR,
                )
                .unwrap();
        }
        renderer.with_context(|gl| unsafe { gl.Finish() }).unwrap();
        let before = super::super::cache::performance::rss_kib();
        let mut d = Desktop::default();
        d.active = true;
        d.output = Some(output.clone());
        d.set_bounds((1646, 926).into());
        d.items = (0..24)
            .map(|i| Item {
                name: format!("File {i:02}.txt"),
                label: format!("File {i:02}.txt"),
                icon: if i % 4 == 0 {
                    "folder"
                } else {
                    "text-x-generic"
                }
                .into(),
                entry: None,
                kind: Kind::File,
            })
            .collect();
        d.repair();
        d.widgets.configure(&["clock".into(), "system".into()]);
        d.widgets.receive(super::super::widgets::sample());
        let mut elements = Vec::new();
        d.append(
            &mut renderer,
            &output,
            &mut elements,
            1.0,
            1.75,
            [0.9, 0.6, 0.4, 1.0],
            "Adwaita",
        );
        {
            let mut frame = renderer.bind(&mut target).unwrap();
            tracker
                .render_output(
                    &mut renderer,
                    &mut frame,
                    0,
                    &elements,
                    crate::render::CLEAR_COLOR,
                )
                .unwrap();
        }
        renderer.with_context(|gl| unsafe { gl.Finish() }).unwrap();
        let loaded = super::super::cache::performance::rss_kib();
        let prepared = d.layer.preparations;
        let allocations = super::super::cache::performance::count(|| {
            for _ in 0..180 {
                elements.clear();
                d.append(
                    &mut renderer,
                    &output,
                    &mut elements,
                    1.0,
                    1.75,
                    [0.9, 0.6, 0.4, 1.0],
                    "Adwaita",
                );
            }
        });
        assert_eq!(allocations, 0);
        assert_eq!(d.layer.preparations, prepared);
        if let Some(directory) = std::env::var_os("GAVNO_CLASSIC_TEST_ARTIFACTS") {
            let path = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&path).unwrap();
            let bytes =
                crate::render::capture_elements(&mut renderer, &elements, (2880, 1620), 1.75)
                    .unwrap();
            crate::debug::write_png_half(&path.join("desktop.png"), 2880, 1620, &bytes).unwrap();
        }
        let label_bytes = d.layer.image_bytes();
        elements.clear();
        drop(d);
        renderer.cleanup_texture_cache().unwrap();
        {
            let mut frame = renderer.bind(&mut target).unwrap();
            tracker
                .render_output(
                    &mut renderer,
                    &mut frame,
                    0,
                    &warm_elements,
                    crate::render::CLEAR_COLOR,
                )
                .unwrap();
        }
        renderer.with_context(|gl| unsafe { gl.Finish() }).unwrap();
        let released = super::super::cache::performance::rss_kib();
        println!("CLASSIC_DESKTOP_RSS_KIB baseline={before} loaded={loaded} delta={} released={released} labels_bytes={label_bytes} static_frame_allocations={allocations}",loaded.saturating_sub(before));
    }
}

#[cfg(test)]
mod editor_tests {
    use super::*;
    #[test]
    fn inline_rename_replaces_selection_and_moves_by_unicode_characters() {
        let mut r = Rename {
            index: 0,
            value: "old.txt".into(),
            cursor: 7,
            select_all: true,
            pending: false,
            error: None,
        };
        r.insert('Ж');
        r.insert('x');
        assert_eq!(r.value, "Жx");
        r.move_cursor(false);
        r.insert('é');
        assert_eq!(r.value, "Жéx");
        r.backspace(false);
        assert_eq!(r.value, "Жx");
        r.delete();
        assert_eq!(r.value, "Ж");
        r.pending = true;
        r.insert('y');
        assert_eq!(r.value, "Ж");
    }
    #[test]
    fn grid_avoids_widget_panels_and_large_repairs_preserve_existing_cells() {
        let mut d = Desktop::default();
        d.set_bounds((1646, 926).into());
        d.widgets.configure(&["clock".into(), "system".into()]);
        d.items = (0..1000)
            .map(|i| Item {
                name: format!("{i}.txt"),
                label: format!("{i}.txt"),
                icon: "text-x-generic".into(),
                entry: None,
                kind: Kind::File,
            })
            .collect();
        assert!(d.repair());
        let positions = d.positions.clone();
        assert!(!d.repair());
        assert_eq!(positions, d.positions);
        for cell in d.positions.icons.values() {
            let local = Cell {
                column: cell.column % d.grid.columns,
                row: cell.row,
            };
            let r = d.grid.rect(local, 0);
            assert!(d
                .widgets
                .layouts(&d.positions.widgets, d.size)
                .all(|(_, panel)| !r.overlaps(panel)));
        }
    }
}
