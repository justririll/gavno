//! The desktop's right-click menu: a Windows-like list with a "New" submenu,
//! "Open with" (the last apps used for that file type first), editors and
//! shortcuts. Rows are built when the menu opens; only hovering changes it.

use super::cache::Layer;
use crate::{
    desktop_entry::DesktopEntry,
    topbar::geometry::{rect, Rect},
};
use smithay::utils::{Logical, Point, Size};

pub const WIDTH: i32 = 268;
pub const SUB_WIDTH: i32 = 248;
const ROW: i32 = 30;
const SEPARATOR: i32 = 9;
const PAD: i32 = 6;

/// What a new desktop entry is created from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewKind {
    Folder,
    /// An empty file without an extension: type the whole name, `main.cpp`.
    File,
    Text,
    /// A file from `~/Templates`.
    Template(String),
}

impl NewKind {
    /// The first free name is this with " 2", " 3", ... before the extension.
    pub fn base_name(&self) -> String {
        match self {
            NewKind::Folder => "New folder".into(),
            NewKind::File => "New file".into(),
            NewKind::Text => "New text document.txt".into(),
            NewKind::Template(name) => name.clone(),
        }
    }
    pub fn contents(&self) -> &'static str {
        ""
    }
    fn label(&self) -> String {
        match self {
            NewKind::Folder => "Folder".into(),
            NewKind::File => "Empty file".into(),
            NewKind::Text => "Text document".into(),
            NewKind::Template(name) => name.clone(),
        }
    }
    fn icon(&self) -> &'static str {
        match self {
            NewKind::Folder => "folder",
            NewKind::File => "text-x-generic",
            NewKind::Text | NewKind::Template(_) => "text-x-generic",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    Open,
    Reveal,
    Rename,
    Trash,
    /// A terminal in the desktop, or in the folder under the menu.
    Terminal,
    Files,
    New(NewKind),
    Refresh,
    Settings,
    /// Open the item (or the desktop) in `editors[i]`.
    Editor(usize),
    /// Open the item with `open_with.apps[i]`.
    OpenWith(usize),
    CopyPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sub {
    New,
    OpenWith,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub action: Option<MenuAction>,
    pub sub: Option<Sub>,
    pub label: String,
    pub hint: &'static str,
    pub icon: String,
}

impl Row {
    fn action(action: MenuAction, label: impl Into<String>, icon: &str, hint: &'static str) -> Self {
        Self { action: Some(action), sub: None, label: label.into(), hint, icon: icon.into() }
    }
    fn sub(sub: Sub, label: &str, icon: &str) -> Self {
        Self { action: None, sub: Some(sub), label: label.into(), hint: "", icon: icon.into() }
    }
    fn separator() -> Self {
        Self { action: None, sub: None, label: String::new(), hint: "", icon: String::new() }
    }
    pub fn is_separator(&self) -> bool {
        self.action.is_none() && self.sub.is_none()
    }
    fn height(&self) -> i32 {
        if self.is_separator() { SEPARATOR } else { ROW }
    }
}

/// An editor found on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub name: &'static str,
    pub command: &'static str,
    pub icon: &'static str,
}

const EDITORS: &[Editor] = &[
    Editor { name: "VS Code", command: "code", icon: "vscode" },
    Editor { name: "VSCodium", command: "codium", icon: "vscodium" },
    Editor { name: "Zed", command: "zeditor", icon: "zed" },
    Editor { name: "CLion", command: "clion", icon: "clion" },
    Editor { name: "IntelliJ IDEA", command: "idea", icon: "idea" },
    Editor { name: "Sublime Text", command: "subl", icon: "sublime-text" },
    Editor { name: "Kate", command: "kate", icon: "kate" },
];

/// Editors installed now: a handful of `stat` calls, done when a menu opens.
pub fn installed_editors() -> Vec<Editor> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<_> = std::env::split_paths(&path).collect();
    EDITORS
        .iter()
        .filter(|e| dirs.iter().any(|d| d.join(e.command).is_file()))
        .cloned()
        .collect()
}

/// "Open with" candidates, filled in by a worker after the menu opened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenWith {
    pub mime: String,
    pub apps: Vec<DesktopEntry>,
    /// How many of `apps` come from the recent list (shown first).
    pub recent: usize,
    pub ready: bool,
}

pub struct Menu {
    pub point: Point<i32, Logical>,
    pub item: Option<usize>,
    /// The item under the menu is a folder.
    pub folder: bool,
    pub rows: Vec<Row>,
    pub hover: Option<usize>,
    pub sub: Option<(Sub, usize)>,
    pub sub_hover: Option<usize>,
    pub editors: Vec<Editor>,
    pub templates: Vec<String>,
    pub open_with: OpenWith,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Row(usize),
    SubRow(usize),
    /// Inside a panel but on padding or a separator.
    Inside,
    Outside,
}

impl Menu {
    /// `item`: `Some((index, real, folder))` for a menu on an icon.
    pub fn new(point: Point<i32, Logical>, item: Option<(usize, bool, bool)>, editors: Vec<Editor>, templates: Vec<String>) -> Self {
        let mut menu = Self {
            point,
            item: item.map(|(i, _, _)| i),
            folder: item.is_some_and(|(_, _, folder)| folder),
            rows: Vec::new(),
            hover: None,
            sub: None,
            sub_hover: None,
            editors,
            templates,
            open_with: OpenWith::default(),
        };
        menu.rows = menu.build(item.is_none_or(|(_, real, _)| real));
        menu
    }

    fn build(&self, real: bool) -> Vec<Row> {
        let editor = |i: usize, e: &Editor, what: &str| Row::action(MenuAction::Editor(i), format!("Open {what}in {}", e.name), e.icon, "");
        let mut rows = Vec::new();
        if self.item.is_none() {
            rows.push(Row::sub(Sub::New, "New", "document-new"));
            rows.push(Row::separator());
            rows.push(Row::action(MenuAction::Terminal, "Open terminal here", "utilities-terminal", ""));
            rows.push(Row::action(MenuAction::Files, "Open in Files", "system-file-manager", ""));
            for (i, e) in self.editors.iter().enumerate().take(2) {
                rows.push(editor(i, e, "desktop "));
            }
            rows.push(Row::separator());
            rows.push(Row::action(MenuAction::Refresh, "Refresh", "view-refresh", "F5"));
            rows.push(Row::action(MenuAction::Settings, "Display settings", "preferences-desktop-display", ""));
            return rows;
        }
        rows.push(Row::action(MenuAction::Open, "Open", "document-open", "Enter"));
        if !real {
            rows.push(Row::action(MenuAction::Reveal, "Open in Files", "system-file-manager", ""));
            return rows;
        }
        rows.push(Row::sub(Sub::OpenWith, "Open with", "applications-other"));
        for (i, e) in self.editors.iter().enumerate().take(2) {
            rows.push(editor(i, e, ""));
        }
        if self.folder {
            rows.push(Row::action(MenuAction::Terminal, "Open terminal here", "utilities-terminal", ""));
        }
        rows.push(Row::action(MenuAction::Reveal, "Show in Files", "system-file-manager", ""));
        rows.push(Row::separator());
        rows.push(Row::action(MenuAction::CopyPath, "Copy path", "edit-copy", ""));
        rows.push(Row::action(MenuAction::Rename, "Rename", "edit", "F2"));
        rows.push(Row::action(MenuAction::Trash, "Move to Trash", "user-trash", "Del"));
        rows
    }

    pub fn sub_rows(&self, sub: Sub) -> Vec<Row> {
        match sub {
            Sub::New => {
                let mut kinds = vec![NewKind::Folder, NewKind::File, NewKind::Text];
                let mut rows: Vec<Row> = kinds.drain(..).map(|k| {
                    let (label, icon) = (k.label(), k.icon());
                    Row::action(MenuAction::New(k), label, icon, "")
                }).collect();
                if !self.templates.is_empty() {
                    rows.push(Row::separator());
                    for name in self.templates.iter().take(8) {
                        let k = NewKind::Template(name.clone());
                        let (label, icon) = (k.label(), k.icon());
                        rows.push(Row::action(MenuAction::New(k), label, icon, ""));
                    }
                }
                rows
            }
            Sub::OpenWith => {
                if !self.open_with.ready {
                    return vec![Row { action: None, sub: None, label: "Looking for apps…".into(), hint: "", icon: String::new() }];
                }
                if self.open_with.apps.is_empty() {
                    return vec![Row { action: None, sub: None, label: "No app opens this".into(), hint: "", icon: String::new() }];
                }
                let mut rows = Vec::new();
                for (i, app) in self.open_with.apps.iter().enumerate() {
                    if i == self.open_with.recent && i > 0 {
                        rows.push(Row::separator());
                    }
                    rows.push(Row::action(MenuAction::OpenWith(i), app.name.clone(), &app.icon, ""));
                }
                rows
            }
        }
    }

    fn rows_height(rows: &[Row]) -> i32 {
        rows.iter().map(Row::height).sum::<i32>() + PAD * 2
    }

    /// The main panel, kept inside `bounds` (the desktop minus the taskbar).
    pub fn rect(&self, bounds: Size<i32, Logical>) -> Rect {
        let height = Self::rows_height(&self.rows);
        rect(
            self.point.x.min((bounds.w - WIDTH - 4).max(0)),
            self.point.y.min((bounds.h - height - 8).max(0)),
            WIDTH,
            height,
        )
    }

    fn row_top(rows: &[Row], panel: Rect, index: usize) -> i32 {
        panel.loc.y + PAD + rows[..index].iter().map(Row::height).sum::<i32>()
    }

    /// The submenu panel beside its row, flipped left when it would not fit.
    pub fn sub_rect(&self, bounds: Size<i32, Logical>) -> Option<Rect> {
        let (sub, row) = self.sub?;
        let panel = self.rect(bounds);
        let rows = self.sub_rows(sub);
        let height = Self::rows_height(&rows);
        let right = panel.loc.x + panel.size.w - 4;
        let x = if right + SUB_WIDTH <= bounds.w { right } else { (panel.loc.x - SUB_WIDTH + 4).max(0) };
        let y = (Self::row_top(&self.rows, panel, row) - PAD).min((bounds.h - height - 8).max(0)).max(0);
        Some(rect(x, y, SUB_WIDTH, height))
    }

    fn row_at(rows: &[Row], panel: Rect, p: Point<f64, Logical>) -> Option<usize> {
        if !panel.to_f64().contains(p) {
            return None;
        }
        let mut y = panel.loc.y + PAD;
        for (i, row) in rows.iter().enumerate() {
            let h = row.height();
            if p.y >= f64::from(y) && p.y < f64::from(y + h) {
                return (!row.is_separator()).then_some(i);
            }
            y += h;
        }
        None
    }

    pub fn hit(&self, p: Point<f64, Logical>, bounds: Size<i32, Logical>) -> Hit {
        if let Some(sub) = self.sub_rect(bounds) {
            if sub.to_f64().contains(p) {
                let rows = self.sub_rows(self.sub.unwrap().0);
                return Self::row_at(&rows, sub, p)
                    .filter(|i| rows[*i].action.is_some())
                    .map_or(Hit::Inside, Hit::SubRow);
            }
        }
        let panel = self.rect(bounds);
        if !panel.to_f64().contains(p) {
            return Hit::Outside;
        }
        Self::row_at(&self.rows, panel, p).map_or(Hit::Inside, Hit::Row)
    }

    /// Pointer motion: returns true when the drawing changed.
    pub fn hover(&mut self, p: Point<f64, Logical>, bounds: Size<i32, Logical>) -> bool {
        let before = (self.hover, self.sub, self.sub_hover);
        match self.hit(p, bounds) {
            Hit::Row(i) => {
                self.hover = Some(i);
                self.sub_hover = None;
                self.sub = self.rows[i].sub.map(|s| (s, i));
            }
            Hit::SubRow(i) => self.sub_hover = Some(i),
            Hit::Inside => self.sub_hover = None,
            // Leaving the menu keeps an open submenu, as a diagonal move
            // toward it often crosses the outside for a moment.
            Hit::Outside => {
                self.hover = self.sub.map(|(_, row)| row);
                self.sub_hover = None;
            }
        }
        before != (self.hover, self.sub, self.sub_hover)
    }

    /// The action of a click at `p`; `None` when the click keeps the menu open
    /// (a submenu row) or does nothing.
    pub fn click(&mut self, p: Point<f64, Logical>, bounds: Size<i32, Logical>) -> Result<Option<MenuAction>, ()> {
        match self.hit(p, bounds) {
            Hit::Row(i) => match &self.rows[i] {
                Row { sub: Some(sub), .. } => {
                    self.sub = Some((*sub, i));
                    Err(())
                }
                Row { action, .. } => Ok(action.clone()),
            },
            Hit::SubRow(i) => Ok(self.sub_rows(self.sub.unwrap().0)[i].action.clone()),
            Hit::Inside => Err(()),
            Hit::Outside => Ok(None),
        }
    }

    pub fn draw(&self, layer: &mut Layer, bounds: Size<i32, Logical>, accent: [f32; 4], theme: &str) {
        let panel = self.rect(bounds);
        draw_panel(layer, panel, &self.rows, self.hover, accent, theme);
        if let (Some((sub, _)), Some(sub_panel)) = (self.sub, self.sub_rect(bounds)) {
            draw_panel(layer, sub_panel, &self.sub_rows(sub), self.sub_hover, accent, theme);
        }
    }
}

fn draw_panel(layer: &mut Layer, panel: Rect, rows: &[Row], hover: Option<usize>, accent: [f32; 4], theme: &str) {
    layer.glass(panel, crate::ui::style::radius::ROW, crate::ui::style::PANEL_BG);
    let mut y = panel.loc.y + PAD;
    for (i, row) in rows.iter().enumerate() {
        let h = row.height();
        if row.is_separator() {
            layer.panel(rect(panel.loc.x + 12, y + h / 2, panel.size.w - 24, 1), 0.0, [1.0, 1.0, 1.0, 0.10], 1.0);
            y += h;
            continue;
        }
        if hover == Some(i) && (row.action.is_some() || row.sub.is_some()) {
            layer.panel(rect(panel.loc.x + 4, y + 1, panel.size.w - 8, h - 2), 6.0, [accent[0], accent[1], accent[2], 0.30], 1.0);
        }
        if !row.icon.is_empty() {
            layer.icon(&row.icon, theme, rect(panel.loc.x + 12, y + 6, 18, 18));
        }
        let dim = row.action.is_none() && row.sub.is_none();
        let mut style = crate::topbar::label_style(12.0);
        if dim {
            style.color = [style.color[0], style.color[1], style.color[2], 0.55];
        }
        let label_w = panel.size.w - 40 - if row.hint.is_empty() && row.sub.is_none() { 12 } else { 64 };
        layer.styled_label(&row.label, rect(panel.loc.x + 38, y + 7, label_w, 18), style, 1, false);
        let hint = if row.sub.is_some() { "›" } else { row.hint };
        if !hint.is_empty() {
            let mut style = crate::topbar::label_style(11.0);
            style.color = [style.color[0], style.color[1], style.color[2], 0.45];
            layer.styled_label(hint, rect(panel.loc.x + panel.size.w - 62, y + 8, 50, 16), style, 1, false);
        }
        y += h;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> Size<i32, Logical> {
        (1600, 900).into()
    }

    #[test]
    fn background_menu_opens_new_on_hover_and_creates_from_its_rows() {
        let mut m = Menu::new((100, 100).into(), None, Vec::new(), vec!["Report.odt".into()]);
        assert_eq!(m.rows[0].sub, Some(Sub::New));
        let panel = m.rect(bounds());
        let first = Point::from((f64::from(panel.loc.x + 40), f64::from(panel.loc.y + PAD + 10)));
        assert!(m.hover(first, bounds()));
        assert_eq!(m.sub, Some((Sub::New, 0)));
        let sub = m.sub_rect(bounds()).unwrap();
        assert!(sub.loc.x >= panel.loc.x + panel.size.w - 4, "opens to the right");
        let file = Point::from((f64::from(sub.loc.x + 40), f64::from(sub.loc.y + PAD + ROW + 10)));
        assert_eq!(m.click(file, bounds()), Ok(Some(MenuAction::New(NewKind::File))));
        // Templates follow after a separator.
        let rows = m.sub_rows(Sub::New);
        assert!(rows.iter().any(|r| r.action == Some(MenuAction::New(NewKind::Template("Report.odt".into())))));
        // Clicking the submenu row itself keeps the menu open.
        assert_eq!(m.click(first, bounds()), Err(()));
        assert_eq!(m.click((5.0, 5.0).into(), bounds()), Ok(None));
    }

    #[test]
    fn submenu_flips_left_at_the_right_edge_and_separators_do_nothing() {
        let mut m = Menu::new((1500, 100).into(), None, Vec::new(), Vec::new());
        let panel = m.rect(bounds());
        assert!(panel.loc.x + panel.size.w <= 1600);
        m.sub = Some((Sub::New, 0));
        let sub = m.sub_rect(bounds()).unwrap();
        assert!(sub.loc.x + sub.size.w <= panel.loc.x + 4);
        let separator = Point::from((f64::from(panel.loc.x + 40), f64::from(panel.loc.y + PAD + ROW + 4)));
        assert_eq!(m.hit(separator, bounds()), Hit::Inside);
    }

    #[test]
    fn item_menus_offer_editors_open_with_and_folder_terminals() {
        let editors = vec![EDITORS[0].clone()];
        let folder = Menu::new((0, 0).into(), Some((3, true, true)), editors.clone(), Vec::new());
        let labels: Vec<_> = folder.rows.iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"Open in VS Code"));
        assert!(labels.contains(&"Open terminal here"));
        assert!(labels.contains(&"Open with"));
        let file = Menu::new((0, 0).into(), Some((3, true, false)), editors, Vec::new());
        assert!(!file.rows.iter().any(|r| r.label == "Open terminal here"));
        let home = Menu::new((0, 0).into(), Some((0, false, false)), Vec::new(), Vec::new());
        assert!(!home.rows.iter().any(|r| r.action == Some(MenuAction::Trash)));
    }

    #[test]
    fn new_kinds_have_sensible_names_and_templates() {
        assert_eq!(NewKind::Folder.base_name(), "New folder");
        assert_eq!(NewKind::File.base_name(), "New file");
        assert!(NewKind::File.contents().is_empty());
        assert_eq!(NewKind::Template("a.odt".into()).base_name(), "a.odt");
    }
}
