//! Persistent dwindle trees and on-demand workspace layout.
use crate::{
    snap::{Direction, Zone},
    window_mode::{mode_state, with_mode_state, WindowMode},
    Gavno,
};
use smithay::{
    desktop::Window,
    output::Output,
    utils::{Logical, Point, Rectangle},
};

pub type Rect = Rectangle<i32, Logical>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
}
impl Axis {
    pub fn flipped(self) -> Self {
        match self {
            Self::X => Self::Y,
            Self::Y => Self::X,
        }
    }
    pub fn position(self, r: Rect) -> i32 {
        match self {
            Self::X => r.loc.x,
            Self::Y => r.loc.y,
        }
    }
    pub fn extent(self, r: Rect) -> i32 {
        match self {
            Self::X => r.size.w,
            Self::Y => r.size.h,
        }
    }
}
#[derive(Clone, Debug)]
enum Node<K> {
    Leaf {
        key: K,
        flipped: bool,
    },
    Split {
        axis: Axis,
        ratio: f64,
        first: Box<Node<K>>,
        second: Box<Node<K>>,
    },
}
#[derive(Clone, Debug)]
pub struct Tree<K> {
    root: Option<Node<K>>,
}
impl<K> Default for Tree<K> {
    fn default() -> Self {
        Self { root: None }
    }
}
#[derive(Clone, Debug)]
pub struct Splitter {
    pub path: Vec<bool>,
    pub axis: Axis,
    pub area: Rect,
    pub cut: i32,
    pub gap: i32,
}
impl Splitter {
    pub fn hit(&self) -> Rect {
        let band = self.gap.max(12);
        let start = self.cut - (band - self.gap) / 2;
        match self.axis {
            Axis::X => Rectangle::new(
                (start, self.area.loc.y).into(),
                (band, self.area.size.h).into(),
            ),
            Axis::Y => Rectangle::new(
                (self.area.loc.x, start).into(),
                (self.area.size.w, band).into(),
            ),
        }
    }
    pub fn line(&self) -> Rect {
        let p = self.cut + self.gap / 2 - 1;
        match self.axis {
            Axis::X => Rectangle::new((p, self.area.loc.y).into(), (2, self.area.size.h).into()),
            Axis::Y => Rectangle::new((self.area.loc.x, p).into(), (self.area.size.w, 2).into()),
        }
    }
}
fn parts(
    r: Rect,
    axis: Axis,
    ratio: f64,
    gap: i32,
    min_a: i32,
    min_b: i32,
) -> (Rect, Rect, i32, i32) {
    let length = axis.extent(r).max(2);
    let gap = gap.max(0).min(length - 2);
    let available = length - gap;
    // On undersized outputs, fit safely rather than produce negative rectangles.
    let a = if available >= min_a.saturating_add(min_b) {
        ((available as f64 * ratio).round() as i32).clamp(min_a, available - min_b)
    } else {
        (available / 2).max(1)
    };
    let mut first = r;
    let mut second = r;
    match axis {
        Axis::X => {
            first.size.w = a;
            second.loc.x += a + gap;
            second.size.w = available - a;
        }
        Axis::Y => {
            first.size.h = a;
            second.loc.y += a + gap;
            second.size.h = available - a;
        }
    }
    (first, second, axis.position(r) + a, gap)
}
impl<K: Clone + PartialEq> Node<K> {
    fn contains(&self, key: &K) -> bool {
        match self {
            Self::Leaf { key: k, .. } => k == key,
            Self::Split { first, second, .. } => first.contains(key) || second.contains(key),
        }
    }
    fn minimum(&self, axis: Axis, gap: i32) -> i32 {
        match self {
            Self::Leaf { .. } => 120,
            Self::Split {
                axis: a,
                first,
                second,
                ..
            } => {
                let (x, y) = (first.minimum(axis, gap), second.minimum(axis, gap));
                if *a == axis {
                    x.saturating_add(y).saturating_add(gap)
                } else {
                    x.max(y)
                }
            }
        }
    }
    fn walk(
        &self,
        r: Rect,
        gap: i32,
        path: &mut Vec<bool>,
        tiles: &mut Vec<(K, Rect)>,
        splits: &mut Vec<Splitter>,
    ) {
        match self {
            Self::Leaf { key, .. } => tiles.push((key.clone(), r)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b, cut, actual_gap) = parts(
                    r,
                    *axis,
                    *ratio,
                    gap,
                    first.minimum(*axis, gap),
                    second.minimum(*axis, gap),
                );
                splits.push(Splitter {
                    path: path.clone(),
                    axis: *axis,
                    area: r,
                    cut,
                    gap: actual_gap,
                });
                path.push(false);
                first.walk(a, gap, path, tiles, splits);
                path.pop();
                path.push(true);
                second.walk(b, gap, path, tiles, splits);
                path.pop();
            }
        }
    }
    fn insert(&mut self, focus: &K, key: K, rect: Rect) -> bool {
        match self {
            Self::Leaf { key: old, flipped } if old == focus => {
                let mut axis = if rect.size.w >= rect.size.h {
                    Axis::X
                } else {
                    Axis::Y
                };
                if *flipped {
                    axis = axis.flipped();
                }
                let first = Box::new(self.clone());
                *self = Self::Split {
                    axis,
                    ratio: 0.5,
                    first,
                    second: Box::new(Self::Leaf {
                        key,
                        flipped: false,
                    }),
                };
                true
            }
            Self::Split { first, second, .. } => {
                if first.contains(focus) {
                    first.insert(focus, key, rect)
                } else {
                    second.insert(focus, key, rect)
                }
            }
            _ => false,
        }
    }
    fn insert_edge(&mut self, target: &K, key: K, dir: Direction) -> bool {
        match self {
            Self::Leaf { key: old, .. } if old==target => {
                let leaf=Self::Leaf {key,flipped:false};
                let original=self.clone();
                let before=matches!(dir,Direction::Left|Direction::Up);
                *self=Self::Split {axis:if matches!(dir,Direction::Left|Direction::Right) {Axis::X} else {Axis::Y}, ratio:0.5,
                    first:Box::new(if before {leaf.clone()} else {original.clone()}),
                    second:Box::new(if before {original} else {leaf})};true
            }
            Self::Split {first,second,..} => if first.contains(target) {first.insert_edge(target,key,dir)} else {second.insert_edge(target,key,dir)},
            _ => false,
        }
    }
    fn remove(self, key: &K) -> Option<Self> {
        match self {
            Self::Leaf { key: ref k, .. } if k == key => None,
            Self::Leaf { .. } => Some(self),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(key), second.remove(key)) {
                (Some(a), Some(b)) => Some(Self::Split {
                    axis,
                    ratio,
                    first: Box::new(a),
                    second: Box::new(b),
                }),
                (a, b) => a.or(b),
            },
        }
    }
    fn rotate(&mut self, key: &K) -> bool {
        match self {
            Self::Leaf { key: k, flipped } => {
                if k != key {
                    return false;
                }
                *flipped = !*flipped;
                true
            }
            Self::Split {
                axis,
                first,
                second,
                ..
            } => {
                let leaf_child = matches!(&**first, Self::Leaf { key: k, .. } if k == key)
                    || matches!(&**second, Self::Leaf { key: k, .. } if k == key);
                if leaf_child {
                    *axis = axis.flipped();
                }
                if first.contains(key) {
                    first.rotate(key)
                } else {
                    second.rotate(key)
                }
            }
        }
    }
    fn swap(&mut self, a: &K, b: &K) {
        match self {
            Self::Leaf { key, .. } => {
                if key == a {
                    *key = b.clone();
                } else if key == b {
                    *key = a.clone();
                }
            }
            Self::Split { first, second, .. } => {
                first.swap(a, b);
                second.swap(a, b);
            }
        }
    }
    /// Preserve absolute positions of every other splitter, including descendants.
    fn restore_cuts(&mut self, r: Rect, gap: i32, path: &mut Vec<bool>, cuts: &[Splitter]) {
        if let Self::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        {
            if let Some(s) = cuts.iter().find(|s| s.path == *path) {
                let available = (axis.extent(r) - gap).max(1);
                *ratio = ((s.cut - axis.position(r)) as f64 / available as f64).clamp(0.0, 1.0);
            }
            let (a, b, _, _) = parts(
                r,
                *axis,
                *ratio,
                gap,
                first.minimum(*axis, gap),
                second.minimum(*axis, gap),
            );
            path.push(false);
            first.restore_cuts(a, gap, path, cuts);
            path.pop();
            path.push(true);
            second.restore_cuts(b, gap, path, cuts);
            path.pop();
        }
    }
}
impl<K: Clone + PartialEq> Tree<K> {
    pub fn contains(&self, key: &K) -> bool {
        self.root.as_ref().is_some_and(|n| n.contains(key))
    }
    pub fn layout(&self, area: Rect, gap: i32) -> (Vec<(K, Rect)>, Vec<Splitter>) {
        let mut tiles = Vec::new();
        let mut splits = Vec::new();
        if let Some(n) = &self.root {
            n.walk(
                Zone::Maximized.rect(area, gap),
                gap.max(0),
                &mut Vec::new(),
                &mut tiles,
                &mut splits,
            );
        }
        (tiles, splits)
    }
    pub fn insert(&mut self, key: K, focus: Option<&K>, area: Rect, gap: i32) {
        if self.contains(&key) {
            return;
        }
        let tiles = self.layout(area, gap).0;
        if let Some((focused, r)) = focus
            .and_then(|f| tiles.iter().find(|(k, _)| k == f))
            .or_else(|| tiles.last())
        {
            if let Some(root) = &mut self.root {
                root.insert(focused, key, *r);
            }
        } else {
            self.root = Some(Node::Leaf {
                key,
                flipped: false,
            });
        }
    }
    pub fn remove(&mut self, key: &K) {
        self.root = self.root.take().and_then(|n| n.remove(key));
    }
    pub fn rotate(&mut self, key: &K) -> bool {
        self.root.as_mut().is_some_and(|n| n.rotate(key))
    }
    pub fn swap(&mut self, a: &K, b: &K) -> bool {
        if a == b || !self.contains(a) || !self.contains(b) {
            return false;
        }
        if let Some(root) = &mut self.root {
            root.swap(a, b);
        }
        true
    }
    pub fn drop_at(&mut self, key: &K, target: &K, edge: Option<Direction>) -> bool {
        if key==target || !self.contains(key) || !self.contains(target) {return false;}
        if let Some(dir)=edge {
            self.remove(key);
            self.root.as_mut().is_some_and(|n|n.insert_edge(target,key.clone(),dir))
        } else {self.swap(key,target)}
    }
    pub fn neighbour(&self, key: &K, dir: Direction, area: Rect, gap: i32) -> Option<K> {
        let tiles = self.layout(area, gap).0;
        let r = tiles.iter().find(|(k, _)| k == key)?.1;
        let candidates: Vec<_> = tiles.iter().filter(|(k, _)| k != key).collect();
        let rects: Vec<_> = candidates.iter().map(|(_, r)| *r).collect();
        crate::snap::neighbour(r, &rects, dir).map(|i| candidates[i].0.clone())
    }
    pub fn side(&self, key: &K, dir: Direction, area: Rect, gap: i32) -> Option<Splitter> {
        let (tiles, splits) = self.layout(area, gap);
        let r = tiles.iter().find(|(k, _)| k == key)?.1;
        splits.into_iter().rev().find(|s| match dir {
            Direction::Left => {
                s.axis == Axis::X
                    && s.cut + s.gap == r.loc.x
                    && r.loc.y >= s.area.loc.y
                    && r.loc.y < s.area.loc.y + s.area.size.h
            }
            Direction::Right => {
                s.axis == Axis::X
                    && s.cut == r.loc.x + r.size.w
                    && r.loc.y >= s.area.loc.y
                    && r.loc.y < s.area.loc.y + s.area.size.h
            }
            Direction::Up => {
                s.axis == Axis::Y
                    && s.cut + s.gap == r.loc.y
                    && r.loc.x >= s.area.loc.x
                    && r.loc.x < s.area.loc.x + s.area.size.w
            }
            Direction::Down => {
                s.axis == Axis::Y
                    && s.cut == r.loc.y + r.size.h
                    && r.loc.x >= s.area.loc.x
                    && r.loc.x < s.area.loc.x + s.area.size.w
            }
        })
    }
    pub fn move_split(&mut self, path: &[bool], delta: i32, area: Rect, gap: i32) -> bool {
        let (tiles, mut splits) = self.layout(area, gap);
        let Some(s) = splits.iter().find(|s| s.path == path).cloned() else {
            return false;
        };
        let mut low = s.axis.position(s.area) + 120;
        let mut high = s.axis.position(s.area) + s.axis.extent(s.area) - s.gap - 120;
        for (_, r) in tiles {
            let p = s.axis.position(r);
            let end = p + s.axis.extent(r);
            // Restrict to descendants of this split's area.
            if !s.area.contains_rect(r) {
                continue;
            }
            if end == s.cut {
                low = low.max(p + 120);
            }
            if p == s.cut + s.gap {
                high = high.min(end - s.gap - 120);
            }
        }
        if low > high {
            return false;
        }
        let cut = s.cut.saturating_add(delta).clamp(low, high);
        if cut == s.cut {
            return false;
        }
        if let Some(target) = splits.iter_mut().find(|s| s.path == path) {
            target.cut = cut;
        }
        if let Some(root) = &mut self.root {
            root.restore_cuts(
                Zone::Maximized.rect(area, gap),
                gap,
                &mut Vec::new(),
                &splits,
            );
        }
        true
    }
}

impl Gavno {
    /// Predict the first tile without reserving a ghost slot for a client that
    /// never maps. Reflow uses the same insertion point when its buffer arrives.
    pub(crate) fn initial_tile_rect(&self, window: &Window) -> Option<Rect> {
        if !self.features.on(crate::module::Feature::Tiling) || self.greeter.is_some() {
            return None;
        }
        let state = mode_state(window);
        if state.mode != WindowMode::Floating || state.tile_floating || state.minimized {
            return None;
        }
        let (output, index) = self.workspaces.membership(window).or_else(|| {
            let output = self.active_output()?;
            let index = self.workspaces.get(&output)?.list.active;
            Some((output, index))
        })?;
        let owner = self.workspaces.get(&output)?;
        let workspace = &owner.list.items[index];
        if !workspace.tiling { return None; }
        let area = self.work_area(&output);
        let mut tree = workspace.tree.clone();
        tree.insert(window.clone(), self.focused_window().as_ref(), area, self.config.gap);
        tree.layout(area, self.config.gap).0.into_iter()
            .find_map(|(w, rect)| (w == *window).then_some(rect))
    }

    pub fn tiling_workspace(&self, window: &Window) -> bool {
        self.features.read(crate::module::Feature::Tiling, Default::default(), || self.tiling_workspace_feature(window))
    }

    fn tiling_workspace_feature(&self, window: &Window) -> bool {
        self.workspaces
            .membership(window)
            .and_then(|(o, i)| self.workspaces.get(&o).map(|o| o.list.items[i].tiling))
            .unwrap_or(false)
    }
    pub fn tiled(&self, window: &Window) -> bool {
        self.features.read(crate::module::Feature::Tiling, Default::default(), || self.tiled_feature(window))
    }

    fn tiled_feature(&self, window: &Window) -> bool {
        let s = mode_state(window);
        self.tiling_workspace(window)
            && !s.tile_floating
            && !s.minimized
            && s.mode == WindowMode::Floating
    }
    fn tile_rect(&self, window: &Window) -> Option<Rect> {
        let loc = self
            .space
            .element_location(window)
            .or_else(|| self.workspaces.location(window))?;
        let size = window
            .toplevel()
            .and_then(|t| t.with_pending_state(|s| s.size))
            .unwrap_or(window.geometry().size);
        Some(Rectangle::new(loc, size))
    }
    pub fn toggle_workspace_tiling(&mut self, output: &Output) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.toggle_workspace_tiling_feature(output))
    }

    fn toggle_workspace_tiling_feature(&mut self, output: &Output) {
        self.end_window_grab();
        self.workspaces.snapshot(&self.space);
        let owner = self.workspaces.ensure(output);
        let snapped: Vec<_> = if !owner.list.items[owner.list.active].tiling {
            owner.list.items[owner.list.active]
                .windows
                .iter()
                .filter(|w| matches!(mode_state(&w.window).mode, WindowMode::Snapped(_)))
                .map(|w| w.window.clone())
                .collect()
        } else {
            Vec::new()
        };
        for w in snapped {
            self.set_window_mode(&w, WindowMode::Floating, None);
        }
        let owner = self.workspaces.ensure(output);
        let ws = &mut owner.list.items[owner.list.active];
        ws.tiling = !ws.tiling;
        if !ws.tiling {
            let windows: Vec<_> = ws.windows.iter().map(|w| w.window.clone()).collect();
            for w in windows {
                if let Some(r) = with_mode_state(&w, |s| s.tile_restore.take()) {
                    if mode_state(&w).mode == WindowMode::Floating {
                        self.apply_tile_rect(&w, r, true, false);
                    } else {
                        with_mode_state(&w, |s| s.restore = Some(r));
                    }
                }
            }
        }
        self.reflow_tiling(true);
        self.queue_redraw_all();
    }
    pub fn toggle_floating(&mut self, window: &Window) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.toggle_floating_feature(window))
    }

    fn toggle_floating_feature(&mut self, window: &Window) {
        if !self.tiling_workspace(window) {
            return;
        }
        with_mode_state(window, |s| s.tile_floating = !s.tile_floating);
        if mode_state(window).tile_floating && mode_state(window).mode != WindowMode::Floating {
            with_mode_state(window, |s| s.restore = s.tile_restore.take().or(s.restore));
        }
        if mode_state(window).tile_floating && mode_state(window).mode == WindowMode::Floating {
            if let Some(r) = with_mode_state(window, |s| s.tile_restore.take()) {
                self.apply_tile_rect(window, r, true, false);
            }
        }
        self.reflow_tiling(true);
        self.raise_tile_overlays();
        self.queue_redraw_all();
    }
    fn reset_tile_rect(&mut self, window: &Window, rect: Rect, animate: bool, tiled: bool) {
        crate::grabs::resize_grab::clear_anchor(window);
        let from = self.tile_rect(window);
        let mapped = self.space.element_location(window).is_some();
        let flags_match = window.toplevel().is_none_or(|t| {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
            t.with_pending_state(|s| s.states.contains(State::TiledLeft) == tiled)
        });
        if !animate && mapped {
            self.features.call(crate::module::Feature::Animations, (), || self.window_animations
                .geometry_immediate(window, rect.to_f64()));
        }
        if from == Some(rect) && flags_match {
            return;
        }
        if let Some(t) = window.toplevel() {
            t.with_pending_state(|s| {
                s.size = Some(rect.size);
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
                for flag in [
                    State::TiledLeft,
                    State::TiledRight,
                    State::TiledTop,
                    State::TiledBottom,
                ] {
                    if tiled {
                        s.states.set(flag);
                    } else {
                        s.states.unset(flag);
                    }
                }
            });
            t.send_pending_configure();
        }
        self.save_workspace_location(window, rect.loc);
        if mapped {
            self.space.map_element(window.clone(), rect.loc, false);
            if animate {
                if let Some(from) = from {
                    crate::anim::animate_geometry(self, window, from);
                }
            }
        }
        self.queue_redraw_all();

        }
    fn apply_tile_rect(&mut self, window: &Window, rect: Rect, animate: bool, tiled: bool) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.apply_tile_rect_feature(window, rect, animate, tiled))
    }

    fn apply_tile_rect_feature(&mut self, window: &Window, rect: Rect, animate: bool, tiled: bool) {
        crate::grabs::resize_grab::clear_anchor(window);
        let from = self.tile_rect(window);
        let mapped = self.space.element_location(window).is_some();
        let flags_match = window.toplevel().is_none_or(|t| {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
            t.with_pending_state(|s| s.states.contains(State::TiledLeft) == tiled)
        });
        if !animate && mapped {
            self.features.call(crate::module::Feature::Animations, (), || self.window_animations
                .geometry_immediate(window, rect.to_f64()));
        }
        if from == Some(rect) && flags_match {
            return;
        }
        if let Some(t) = window.toplevel() {
            t.with_pending_state(|s| {
                s.size = Some(rect.size);
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
                for flag in [
                    State::TiledLeft,
                    State::TiledRight,
                    State::TiledTop,
                    State::TiledBottom,
                ] {
                    if tiled {
                        s.states.set(flag);
                    } else {
                        s.states.unset(flag);
                    }
                }
            });
            t.send_pending_configure();
        }
        self.save_workspace_location(window, rect.loc);
        if mapped {
            self.space.map_element(window.clone(), rect.loc, false);
            if animate {
                if let Some(from) = from {
                    crate::anim::animate_geometry(self, window, from);
                }
            }
        }
        self.queue_redraw_all();
    }
    pub fn reflow_tiling(&mut self, animate: bool) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.reflow_tiling_feature(animate))
    }

    fn reflow_tiling_feature(&mut self, animate: bool) {
        if self.greeter.is_some() {
            return;
        }
        let focus = self.focused_window();
        let moving = self.workspaces.moving_tile.clone();
        let mut targets = Vec::new();
        let mut restores = Vec::new();
        for index in 0..self.workspaces.outputs.len() {
            let area = self.work_area(&self.workspaces.outputs[index].output);
            let gap = self.config.gap;
            for ws in &mut self.workspaces.outputs[index].list.items {
                for (w, _) in ws.tree.layout(area, gap).0 {
                    if !ws.windows.iter().any(|v| v.window == w) {
                        ws.tree.remove(&w);
                    }
                }
                if !ws.tiling {
                    for w in &ws.windows {
                        if let Some(r) = with_mode_state(&w.window, |s| s.tile_restore.take()) {
                            if mode_state(&w.window).mode == WindowMode::Floating {
                                restores.push((w.window.clone(), r));
                            } else {
                                with_mode_state(&w.window, |s| s.restore = Some(r));
                            }
                        }
                    }
                    continue;
                }
                let eligible: Vec<_> = ws
                    .windows
                    .iter()
                    .filter(|w| {
                        let s = mode_state(&w.window);
                        !s.minimized && !s.tile_floating && s.mode == WindowMode::Floating
                    })
                    .map(|w| w.window.clone())
                    .collect();
                for (w, _) in ws.tree.layout(area, gap).0 {
                    if !eligible.contains(&w) {
                        ws.tree.remove(&w);
                    }
                }
                for w in eligible {
                    let saved = ws.windows.iter().find(|v| v.window == w).map(|v| {
                        let size = w
                            .toplevel()
                            .and_then(|t| t.with_pending_state(|s| s.size))
                            .unwrap_or(w.geometry().size);
                        Rectangle::new(v.location, size)
                    });
                    with_mode_state(&w, |s| {
                        if s.tile_restore.is_none() {
                            s.tile_restore = saved;
                        }
                    });
                    if !ws.tree.contains(&w) {
                        ws.tree.insert(w, focus.as_ref(), area, gap);
                    }
                }
                targets.extend(ws.tree.layout(area, gap).0);
            }
        }
        for (w, r) in restores {
            self.apply_tile_rect(&w, r, animate, false);
        }
        for (w, r) in targets {
            if moving.as_ref() != Some(&w) {
                self.apply_tile_rect(&w, r, animate, true);
            }
        }
        self.raise_tile_overlays();
    }
    pub fn raise_tile_overlays(&mut self) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.raise_tile_overlays_feature())
    }

    fn raise_tile_overlays_feature(&mut self) {
        let windows: Vec<_> = self
            .space
            .elements()
            .filter(|w| self.tiling_workspace(w) && !self.tiled(w))
            .cloned()
            .collect();
        for w in windows {
            self.space.raise_element(&w, false);
        }
    }
    pub fn rotate_tile(&mut self) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.rotate_tile_feature())
    }

    fn rotate_tile_feature(&mut self) {
        let Some(w) = self.focused_window().filter(|w| self.tiled(w)) else {
            return;
        };
        let Some((o, i)) = self.workspaces.membership(&w) else {
            return;
        };
        self.workspaces.ensure(&o).list.items[i].tree.rotate(&w);
        self.reflow_tiling(true);
    }
    pub fn move_tile(&mut self, dir: Direction) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.move_tile_feature(dir))
    }

    fn move_tile_feature(&mut self, dir: Direction) {
        let Some(w) = self.focused_window().filter(|w| self.tiled(w)) else {
            return;
        };
        self.move_tile_window(&w,dir);
    }
    pub fn move_tile_window(&mut self,w:&Window,dir:Direction) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.move_tile_window_feature(w, dir))
    }

    fn move_tile_window_feature(&mut self,w:&Window,dir:Direction) {
        let Some((o,i))=self.workspaces.membership(w) else {return};
        let area=self.work_area(&o);let gap=self.config.gap;
        let tree=&mut self.workspaces.ensure(&o).list.items[i].tree;
        if let Some(n)=tree.neighbour(w,dir,area,gap) {tree.swap(w,&n);}
        // The dragged window has a temporary position; animate it back to its slot.
        if let Some((_,r))=tree.layout(area,gap).0.into_iter().find(|(n,_)| n==w) {self.apply_tile_rect(w,r,true,true);}
        self.reflow_tiling(true);
    }
    pub fn resize_tile(&mut self, w: &Window, dir: Direction, delta: i32, animate: bool) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.resize_tile_feature(w, dir, delta, animate))
    }

    fn resize_tile_feature(&mut self, w: &Window, dir: Direction, delta: i32, animate: bool) {
        let Some((o, i)) = self.workspaces.membership(w) else {
            return;
        };
        let area = self.work_area(&o);
        let gap = self.config.gap;
        let tree = &mut self.workspaces.ensure(&o).list.items[i].tree;
        if let Some(s) = tree.side(w, dir, area, gap) {
            tree.move_split(&s.path, delta, area, gap);
        }
        self.reflow_tiling(animate);
    }
    pub fn drop_tile(&mut self, w: &Window, pos: Point<f64, Logical>) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.drop_tile_feature(w, pos))
    }

    fn drop_tile_feature(&mut self, w: &Window, pos: Point<f64, Logical>) {
        let Some((o, i)) = self.workspaces.membership(w) else {
            return;
        };
        let area = self.work_area(&o);
        let gap = self.config.gap;
        let tree = &mut self.workspaces.ensure(&o).list.items[i].tree;
        if let Some((n, r)) = tree
            .layout(area, gap)
            .0
            .into_iter()
            .find(|(n, r)| n != w && r.to_f64().contains(pos))
        {
            tree.drop_at(w, &n, drop_edge(r,pos));
        }
        // The grab still exists during release; explicitly restore its own slot.
        if let Some((_, r)) = tree.layout(area, gap).0.into_iter().find(|(n, _)| n == w) {
            self.apply_tile_rect(w, r, true, true);
        }
        self.reflow_tiling(true);
    }
    pub fn splitter_at(&self, pos: Point<f64, Logical>) -> Option<(Output, Splitter)> {
        self.features.read(crate::module::Feature::Tiling, Default::default(), || self.splitter_at_feature(pos))
    }

    fn splitter_at_feature(&self, pos: Point<f64, Logical>) -> Option<(Output, Splitter)> {
        if self.greeter.is_some()
            || self.protocols.locks.locked
            || self.shell_contains(pos)
            || self.desktop_transform_at(pos)
        {
            return None;
        }
        use smithay::wayland::shell::wlr_layer::Layer;
        if self.layer_hit(pos, &[Layer::Overlay, Layer::Top]).is_some() {
            return None;
        }
        if self
            .space
            .element_under(pos)
            .is_some_and(|(w, _)| !self.tiled(w))
        {
            return None;
        }
        if self.space.elements().any(|w| {
            self.tiling_workspace(w)
                && !self.tiled(w)
                && self.tile_rect(w).is_some_and(|r| r.to_f64().contains(pos))
        }) {
            return None;
        }
        for o in &self.workspaces.outputs {
            let ws = &o.list.items[o.list.active];
            if !ws.tiling {
                continue;
            }
            let (_, splits) = ws.tree.layout(self.work_area(&o.output), self.config.gap);
            if let Some(s) = splits
                .into_iter()
                .rev()
                .find(|s| s.hit().to_f64().contains(pos))
            {
                return Some((o.output.clone(), s));
            }
        }
        None
    }
}

impl Gavno {
    pub fn refresh_splitter_cursor(&mut self) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.refresh_splitter_cursor_feature())
    }

    fn refresh_splitter_cursor_feature(&mut self) {
        if self.greeter.is_some() || self.protocols.locks.locked {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        if pointer.is_grabbed() {
            return;
        }
        let old = self.workspaces.splitter_hover.take();
        let hit = self.splitter_at(pointer.current_location());
        if let Some((_, s)) = &hit {
            use smithay::input::pointer::{CursorIcon, CursorImageStatus};
            self.cursor_status = CursorImageStatus::Named(match s.axis {
                Axis::X => CursorIcon::ColResize,
                Axis::Y => CursorIcon::RowResize,
            });
        } else if old.is_some() {
            self.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
        }
        let key = |v: &Option<(Output, Splitter)>| {
            v.as_ref().map(|(o, s)| (o.clone(), s.path.clone(), s.cut))
        };
        if key(&old) != key(&hit) {
            self.queue_redraw_all();
        }
        self.workspaces.splitter_hover = hit;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rectangle::new((x, y).into(), (w, h).into())
    }
    fn area() -> Rect {
        r(0, 0, 1600, 1000)
    }
    fn chain(n: u32, gap: i32) -> Tree<u32> {
        let mut t = Tree::default();
        for i in 0..n {
            t.insert(i, i.checked_sub(1).as_ref(), area(), gap);
        }
        t
    }
    #[test]
    fn dwindle_one_to_six_with_outer_and_inner_gaps() {
        for gap in [0, 12, 24] {
            for count in 1..=6 {
                let (tiles, splits) = chain(count, gap).layout(area(), gap);
                assert_eq!(tiles.len(), count as usize);
                assert_eq!(splits.len(), count as usize - 1);
                let outer = Zone::Maximized.rect(area(), gap);
                for (i, (_, rect)) in tiles.iter().enumerate() {
                    assert!(outer.contains_rect(*rect));
                    assert!(rect.size.w >= 120 && rect.size.h >= 120, "{rect:?}");
                    for (_, other) in &tiles[i + 1..] {
                        assert!(!rect.overlaps(*other));
                    }
                }
                let tile_area: i64 = tiles
                    .iter()
                    .map(|(_, r)| i64::from(r.size.w) * i64::from(r.size.h))
                    .sum();
                let gap_area: i64 = splits
                    .iter()
                    .map(|s| i64::from(s.gap) * i64::from(s.axis.flipped().extent(s.area)))
                    .sum();
                assert_eq!(
                    tile_area + gap_area,
                    i64::from(outer.size.w) * i64::from(outer.size.h)
                );
            }
        }
        assert_eq!(
            chain(2, 12).layout(area(), 12).0,
            vec![(0, r(12, 12, 782, 976)), (1, r(806, 12, 782, 976))]
        );
        assert_eq!(
            chain(3, 12).layout(area(), 12).0,
            vec![
                (0, r(12, 12, 782, 976)),
                (1, r(806, 12, 782, 482)),
                (2, r(806, 506, 782, 482))
            ]
        );
    }
    #[test]
    fn focused_insertion_flip_rotation_and_parent_collapse() {
        let mut t = chain(1, 12);
        assert!(t.rotate(&0));
        t.insert(1, Some(&0), area(), 12);
        assert_eq!(t.layout(area(), 12).1[0].axis, Axis::Y);
        t.insert(2, Some(&0), area(), 12);
        assert_eq!(
            t.layout(area(), 12)
                .0
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>(),
            [0, 2, 1]
        );
        t.insert(2, Some(&0), area(), 12);
        assert_eq!(t.layout(area(), 12).0.len(), 3);
        t.remove(&2);
        assert_eq!(t.layout(area(), 12).1.len(), 1);
        t.remove(&0);
        assert_eq!(t.layout(area(), 12).0, vec![(1, r(12, 12, 1576, 976))]);
        t.remove(&1);
        t.remove(&99);
        assert!(t.layout(area(), 12).0.is_empty());
    }
    #[test]
    fn dragging_splitter_changes_only_adjacent_tiles_and_anchors_outer_edges() {
        let mut t = chain(4, 12);
        let (before, splits) = t.layout(area(), 12);
        let s = splits.last().unwrap();
        assert!(t.move_split(&s.path, 40, area(), 12));
        let (after, _) = t.layout(area(), 12);
        assert_eq!(before[..2], after[..2]);
        assert_eq!(before[2].1.loc, after[2].1.loc);
        assert_eq!(
            before[3].1.loc.x + before[3].1.size.w,
            after[3].1.loc.x + after[3].1.size.w
        );
        assert_eq!(after[2].1.size.w - before[2].1.size.w, 40);
        assert_eq!(after[3].1.size.w - before[3].1.size.w, -40);
        // Moving an ancestor must leave every descendant divider at its old coordinate.
        let (_, before_splits) = t.layout(area(), 12);
        assert!(t.move_split(&[], 40, area(), 12));
        let (_, after_splits) = t.layout(area(), 12);
        for (a, b) in before_splits.iter().zip(&after_splits).skip(1) {
            assert_eq!(a.cut, b.cut);
        }
    }
    #[test]
    fn clamps_ratios_against_every_adjacent_leaf() {
        for count in 2..=6 {
            for amount in [i32::MIN, i32::MAX] {
                let mut t = chain(count, 12);
                for s in t.layout(area(), 12).1 {
                    t.move_split(&s.path, amount, area(), 12);
                    for (_, r) in t.layout(area(), 12).0 {
                        assert!(r.size.w >= 120 && r.size.h >= 120, "{r:?}");
                    }
                }
            }
        }
        let mut t = chain(2, 12);
        t.move_split(&[], i32::MAX, area(), 12);
        assert_eq!(t.layout(area(), 12).0[1].1.size.w, 120);
        assert!(!t.move_split(&[], 1, area(), 12));
        t.move_split(&[], i32::MIN, area(), 12);
        assert_eq!(t.layout(area(), 12).0[0].1.size.w, 120);
    }
    #[test]
    fn swap_by_direction_moves_keys_and_keeps_slots() {
        let mut t = chain(3, 12);
        assert_eq!(t.neighbour(&0, Direction::Left, area(), 12), None);
        assert_eq!(t.neighbour(&1, Direction::Down, area(), 12), Some(2));
        let before = t.layout(area(), 12).0;
        let next = t.neighbour(&0, Direction::Right, area(), 12).unwrap();
        assert!(t.swap(&0, &next));
        let after = t.layout(area(), 12).0;
        assert_eq!(
            before.iter().map(|(_, r)| r).collect::<Vec<_>>(),
            after.iter().map(|(_, r)| r).collect::<Vec<_>>()
        );
        assert_eq!(after[0].0, next);
        assert!(!t.swap(&0, &99));
    }
    #[test]
    fn splitter_hit_covers_full_gap_and_twelve_pixels_with_zero_gap() {
        for gap in [0, 4, 12, 24] {
            let s = chain(2, gap).layout(area(), gap).1.remove(0);
            assert_eq!(s.hit().size.w, gap.max(12));
            assert_eq!(s.hit().size.h, s.area.size.h);
            assert!(s.hit().contains((s.cut, s.area.loc.y)));
        }
    }
    fn output(h: &mut crate::state::tests::Harness) -> Output {
        let o = Output::new(
            "test".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        o.change_current_state(
            Some(smithay::output::Mode {
                size: (1600, 1000).into(),
                refresh: 120000,
            }),
            None,
            None,
            None,
        );
        h.gv.space.map_output(&o, (0, 0));
        o
    }
    fn window(h: &mut crate::state::tests::Harness, rect: Rect) -> Window {
        let w = h.window();
        h.gv.map_new_window(w.clone());
        h.gv.space.map_element(w.clone(), rect.loc, false);
        w.toplevel()
            .unwrap()
            .with_pending_state(|s| s.size = Some(rect.size));
        h.gv.save_workspace_location(&w, rect.loc);
        w
    }
    #[test]
    fn first_configure_matches_first_and_subsequent_tiles_without_resize() {
        use smithay::wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData};
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
        let mut h = crate::state::tests::Harness::new();
        let output = output(&mut h);
        h.gv.workspaces.ensure(&output).list.items[0].tiling = true;
        for _ in 0..3 {
            let window = h.window();
            let expected = h.gv.initial_tile_rect(&window).unwrap();
            crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &window);
            let t = window.toplevel().unwrap();
            t.with_pending_state(|s| {
                assert_eq!(s.size, Some(expected.size));
                assert!(s.states.contains(State::TiledLeft));
                assert!(s.states.contains(State::Activated));
            });
            t.send_configure();
            h.gv.map_new_window(window.clone());
            assert_eq!(h.gv.space.element_location(&window), Some(expected.loc));
            assert_eq!(t.with_pending_state(|s| s.size), Some(expected.size));
            with_states(t.wl_surface(), |states| {
                assert_eq!(states.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap().pending_configures().len(), 1,
                    "the new client must not receive a second configure when it maps");
            });
        }
    }

    #[test]
    fn initial_configure_preserves_floating_size_and_pre_map_maximize() {
        let mut h = crate::state::tests::Harness::new();
        let output = output(&mut h);
        h.gv.workspaces.ensure(&output);
        let window = h.window();
        crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &window);
        assert_eq!(window.toplevel().unwrap().with_pending_state(|s| s.size), None);
        h.gv.set_window_mode(&window, WindowMode::Maximized, Some(output));
        let size = window.toplevel().unwrap().with_pending_state(|s| s.size);
        assert!(size.is_some());
        crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &window);
        assert_eq!(window.toplevel().unwrap().with_pending_state(|s| s.size), size);
    }

    #[test]
    fn initial_classic_configure_includes_final_decoration_and_maximized_size() {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        let mut h = crate::state::tests::Harness::new();
        let output = output(&mut h);
        h.gv.classic.settled = crate::classic::ShellMode::Classic;
        h.gv.workspaces.ensure(&output);
        let window = h.window();
        let t = window.toplevel().unwrap();
        t.with_pending_state(|s| s.decoration_mode = Some(Mode::ClientSide));
        h.gv.set_window_mode(&window, WindowMode::Maximized, Some(output.clone()));
        crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &window);
        let expected = h.gv.classic_client_rect(&window, Zone::Maximized.rect(h.gv.work_area(&output), 0));
        t.with_pending_state(|s| {
            assert_eq!(s.decoration_mode, Some(Mode::ServerSide));
            assert_eq!(s.size, Some(expected.size));
        });
        t.send_configure();
        assert!(!h.gv.sync_classic_decorations());
        h.gv.map_new_window(window.clone());
        assert_eq!(h.gv.space.element_location(&window), Some(expected.loc));
        assert!(t.send_pending_configure().is_none());
    }

    #[test]
    fn workspace_toggle_round_trip_restores_floating_geometry_and_settles() {
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h);
        let original = [r(50, 100, 500, 300), r(200, 250, 600, 400)];
        let windows = [window(&mut h, original[0]), window(&mut h, original[1])];
        for _ in 0..2 {
            h.gv.toggle_workspace_tiling(&o);
            for w in &windows {
                assert!(h.gv.tiled(w));
            }
            h.gv.toggle_workspace_tiling(&o);
            for (w, r) in windows.iter().zip(original) {
                assert_eq!(h.gv.tile_rect(w), Some(r));
                assert_eq!(mode_state(w).tile_restore, None);
            }
        }
        for i in 1..500 {
            h.gv.window_animations
                .advance(std::time::Duration::from_millis(i * 8), &h.gv.space);
        }
        assert!(!h.gv.window_animations.is_animating());
        h.gv.reflow_tiling(true);
        assert!(!h.gv.window_animations.is_animating());
    }
    #[test]
    fn live_splitter_grab_tracks_pointer_without_springs_and_can_be_interrupted() {
        use smithay::input::pointer::{Focus, GrabStartData, MotionEvent};
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h);
        let a = window(&mut h, r(50, 80, 500, 400));
        let b = window(&mut h, r(100, 150, 600, 450));
        h.gv.toggle_workspace_tiling(&o);
        let area = h.gv.work_area(&o);
        let gap = h.gv.config.gap;
        let (before, splits) = h.gv.workspaces.get(&o).unwrap().list.items[0]
            .tree
            .layout(area, gap);
        let split = splits[0].clone();
        let start = Point::from((split.cut as f64 + 2.0, 300.0));
        assert!(h.gv.splitter_at(start).is_some());
        let pointer = h.gv.seat.get_pointer().unwrap();
        let grab = crate::grabs::tiling_grab::SplitterGrab::new(
            GrabStartData {
                focus: None,
                button: 0x110,
                location: start,
            },
            o.clone(),
            split,
        );
        pointer.set_grab(
            &mut h.gv,
            grab,
            smithay::utils::SERIAL_COUNTER.next_serial(),
            Focus::Clear,
        );
        pointer.motion(
            &mut h.gv,
            None,
            &MotionEvent {
                location: start + Point::from((40.0, 0.0)),
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: 1,
            },
        );
        let after = h.gv.workspaces.get(&o).unwrap().list.items[0]
            .tree
            .layout(area, gap)
            .0;
        assert_eq!(after[0].1.size.w - before[0].1.size.w, 40);
        for w in [&a, &b] {
            assert!(!h.gv.window_animations.motion(w).unwrap().active());
        }
        for w in [&a, &b] {
            let target = h.gv.tile_rect(w).unwrap().to_f64();
            assert_eq!(
                h.gv.window_animations
                    .motion(w)
                    .unwrap()
                    .rect(r(0, 0, 1, 1).to_f64()),
                target
            );
            assert!(!w.toplevel().unwrap().has_pending_changes());
        }
        h.gv.refresh_workspaces();
        assert!(!h.gv.window_animations.is_animating());
        assert_eq!(
            h.gv.workspaces.splitter_hover.as_ref().unwrap().1.cut,
            after[0].1.loc.x + after[0].1.size.w
        );
        h.gv.switch_workspace(&o, 1);
        assert!(!pointer.is_grabbed());
        assert_eq!(h.gv.space.elements().count(), 0);
    }
    #[test]
    fn title_drag_reorders_and_does_not_snap_or_reflow_under_the_pointer() {
        use smithay::input::pointer::{ButtonEvent, Focus, GrabStartData, MotionEvent};
        use smithay::{backend::input::ButtonState, utils::SERIAL_COUNTER};
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h);
        let a = window(&mut h, r(50, 80, 500, 400));
        let b = window(&mut h, r(100, 150, 600, 450));
        h.gv.toggle_workspace_tiling(&o);
        let area = h.gv.work_area(&o);
        let gap = h.gv.config.gap;
        let before = h.gv.workspaces.get(&o).unwrap().list.items[0]
            .tree
            .layout(area, gap)
            .0;
        let destination = before.iter().find(|(w, _)| w == &b).unwrap().1;
        let initial = h.gv.space.element_location(&a).unwrap();
        let start = initial.to_f64() + Point::from((30.0, 30.0));
        let end = destination.loc.to_f64() + Point::from((destination.size.w as f64 / 2.0, destination.size.h as f64 / 2.0));
        let pointer = h.gv.seat.get_pointer().unwrap();
        let grab = crate::grabs::MoveSurfaceGrab::start(
            GrabStartData {
                focus: None,
                button: 0x110,
                location: start,
            },
            a.clone(),
            initial,
        );
        pointer.set_grab(&mut h.gv, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
        pointer.button(
            &mut h.gv,
            &ButtonEvent {
                button: 0x110,
                state: ButtonState::Pressed,
                serial: SERIAL_COUNTER.next_serial(),
                time: 1,
            },
        );
        pointer.motion(
            &mut h.gv,
            None,
            &MotionEvent {
                location: end,
                serial: SERIAL_COUNTER.next_serial(),
                time: 2,
            },
        );
        let dragged = h.gv.space.element_location(&a);
        h.gv.refresh_workspaces();
        assert_eq!(h.gv.space.element_location(&a), dragged);
        pointer.button(
            &mut h.gv,
            &ButtonEvent {
                button: 0x110,
                state: ButtonState::Released,
                serial: SERIAL_COUNTER.next_serial(),
                time: 3,
            },
        );
        assert!(!pointer.is_grabbed());
        assert!(h.gv.workspaces.moving_tile.is_none());
        let after = h.gv.workspaces.get(&o).unwrap().list.items[0]
            .tree
            .layout(area, gap)
            .0;
        assert_eq!(after[0].0, b);
        assert_eq!(after[1].0, a);
        assert_eq!(h.gv.tile_rect(&a), Some(destination));
        assert_eq!(mode_state(&a).mode, WindowMode::Floating);
    }
    #[test]
    fn configured_default_applies_to_initial_and_trailing_workspaces() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.workspaces.default_tiling = true;
        let o = output(&mut h);
        let a = window(&mut h, r(50, 80, 500, 400));
        assert!(h.gv.tiled(&a));
        assert!(h
            .gv
            .workspaces
            .get(&o)
            .unwrap()
            .list
            .items
            .iter()
            .all(|w| w.tiling));
        h.gv.switch_workspace(&o, 1);
        let b = window(&mut h, r(150, 180, 500, 400));
        assert!(h.gv.tiled(&b));
        assert_eq!(h.gv.space.elements().count(), 1);
        h.gv.switch_workspace(&o, 0);
        assert_eq!(h.gv.space.elements().count(), 1);
        assert!(h.gv.tiled(&a));
    }
    #[test]
    fn outer_tile_edges_do_not_move_an_opposite_splitter_by_keyboard_or_grab() {
        use crate::grabs::{resize_grab::ResizeEdge, tiling_grab::SplitterGrab};
        use smithay::input::pointer::{Focus, GrabStartData, MotionEvent};
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h);
        for _ in 0..4 { window(&mut h, r(100, 100, 500, 400)); }
        h.gv.toggle_workspace_tiling(&o);
        let area = h.gv.work_area(&o);
        let gap = h.gv.config.gap;
        let before = h.gv.workspaces.get(&o).unwrap().list.items[0].tree.layout(area, gap).0;
        let pointer = h.gv.seat.get_pointer().unwrap();
        let mut tested = 0;
        for (w, rect) in &before {
            for (dir, edge) in [(Direction::Left, ResizeEdge::LEFT), (Direction::Right, ResizeEdge::RIGHT), (Direction::Up, ResizeEdge::TOP), (Direction::Down, ResizeEdge::BOTTOM)] {
                if h.gv.workspaces.get(&o).unwrap().list.items[0].tree.side(w, dir, area, gap).is_some() { continue; }
                tested += 1;
                h.gv.resize_nearest_split(w, dir, 75);
                assert!(h.gv.tile_resize_edges(w, edge).is_empty());
                let start = rect.loc.to_f64();
                let grab = SplitterGrab::for_tile(&h.gv, GrabStartData { focus: None, button: 0x110, location: start }, w, edge).unwrap();
                pointer.set_grab(&mut h.gv, grab, smithay::utils::SERIAL_COUNTER.next_serial(), Focus::Clear);
                pointer.motion(&mut h.gv, None, &MotionEvent { location: start + Point::from((100.0, 100.0)), serial: smithay::utils::SERIAL_COUNTER.next_serial(), time: 10 });
                h.gv.end_window_grab();
                assert_eq!(h.gv.workspaces.get(&o).unwrap().list.items[0].tree.layout(area, gap).0, before);
            }
        }
        assert!(tested >= 4);
    }
    #[test]
    fn lifecycle_minimize_fixed_modes_override_and_workspace_migration() {
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h);
        let a = window(&mut h, r(50, 80, 500, 400));
        let b = window(&mut h, r(100, 150, 600, 450));
        h.gv.toggle_workspace_tiling(&o);
        h.gv.toggle_floating(&b);
        assert!(!h.gv.tiled(&b));
        assert_eq!(h.gv.tile_rect(&b), Some(r(100, 150, 600, 450)));
        assert_eq!(
            h.gv.workspaces.get(&o).unwrap().list.items[0]
                .tree
                .layout(area(), 12)
                .0
                .len(),
            1
        );
        h.gv.toggle_floating(&b);
        h.gv.set_window_mode(&b, WindowMode::Maximized, None);
        assert!(!h.gv.tiled(&b));
        assert_eq!(h.gv.space.elements().next_back(), Some(&b));
        h.gv.set_window_mode(&b, WindowMode::Floating, None);
        assert!(h.gv.tiled(&b));
        h.gv.minimize_window(&b);
        assert!(!h.gv.workspaces.get(&o).unwrap().list.items[0]
            .tree
            .contains(&b));
        h.gv.focus_window(Some(&b));
        assert!(h.gv.tiled(&b));
        assert!(h.gv.move_to_workspace(&a, 1));
        assert_eq!(h.gv.tile_rect(&a), Some(r(50, 80, 500, 400)));
        assert!(!h.gv.workspaces.get(&o).unwrap().list.items[0]
            .tree
            .contains(&a));
        h.gv.workspace_forget(&b);
        assert!(h.gv.workspaces.get(&o).unwrap().list.items[0]
            .tree
            .layout(area(), 12)
            .0
            .is_empty());
    }
}

/// The middle half swaps, the nearest outer quarter creates a new split.
pub fn drop_edge(r:Rect,p:Point<f64,Logical>) -> Option<Direction> {
    let x=(p.x-r.loc.x as f64)/r.size.w.max(1) as f64;
    let y=(p.y-r.loc.y as f64)/r.size.h.max(1) as f64;
    [(x,Direction::Left),(1.0-x,Direction::Right),(y,Direction::Up),(1.0-y,Direction::Down)]
        .into_iter().min_by(|a,b|a.0.total_cmp(&b.0)).filter(|(d,_)| *d<0.25).map(|(_,d)|d)
}
impl Gavno {
    pub fn resize_nearest_split(&mut self,w:&Window,dir:Direction,delta:i32) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.resize_nearest_split_feature(w, dir, delta))
    }

    fn resize_nearest_split_feature(&mut self,w:&Window,dir:Direction,delta:i32) {
        let Some((o,i))=self.workspaces.membership(w) else {return};
        let area=self.work_area(&o);let gap=self.config.gap;
        let tree=&mut self.workspaces.ensure(&o).list.items[i].tree;
        if let Some(s)=tree.side(w,dir,area,gap) {
            if tree.move_split(&s.path,delta,area,gap) {self.reflow_tiling(false);}
        }
    }
    pub fn preview_tile_drop(&mut self,w:&Window,pos:Point<f64,Logical>) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.preview_tile_drop_feature(w, pos))
    }

    fn preview_tile_drop_feature(&mut self,w:&Window,pos:Point<f64,Logical>) {
        let Some((o,i))=self.workspaces.membership(w) else {return};
        let area=self.work_area(&o);let gap=self.config.gap;
        let tree=&self.workspaces.ensure(&o).list.items[i].tree;
        let target=tree.layout(area,gap).0.into_iter().find(|(n,r)|n!=w && r.to_f64().contains(pos));
        if let Some((n,r))=target {
            let mut projected=tree.clone();
            projected.drop_at(w,&n,drop_edge(r,pos));
            let preview=projected.layout(area,gap).0.into_iter().find(|(n,_)|n==w).map(|(_,r)|r).unwrap_or(r);
            let from=self.interactive_rect(w).unwrap_or(preview);
            self.features.call(crate::module::Feature::Switcher, (), || self.overlay.preview(o,Zone::Maximized,from,preview,self.start_time.elapsed()));
        } else {self.features.call(crate::module::Feature::Switcher, (), || self.overlay.hide_preview(self.start_time.elapsed()));}
    }
}

impl Gavno {
    pub(crate) fn reset_tiling(&mut self) {
        self.workspaces.default_tiling = false;
        self.workspaces.moving_tile = None;
        self.workspaces.splitter_hover = None;
        let mut windows: Vec<_> = self.space.elements().cloned().collect();
        for owner in &mut self.workspaces.outputs {
            owner.list.default_tiling = false;
            for ws in &mut owner.list.items {
                ws.tiling = false;
                ws.tree = Default::default();
                for v in &ws.windows { if !windows.contains(&v.window) { windows.push(v.window.clone()); } }
            }
        }
        for w in windows {
            let s = mode_state(&w);
            if s.tile_restore.is_none() { continue; }
            let rect = self.window_animations.motion(&w).map(|m| m.rect(self.tile_rect(&w).unwrap_or_default().to_f64()).to_i32_round())
                .or_else(|| self.tile_rect(&w));
            with_mode_state(&w, |s| { s.tile_restore = None; s.tile_floating = true; });
            if let Some(mut r) = rect {
                // Recovery can run inside a pointer grab callback. Use ownership,
                // never active_output(), which would reenter the pointer mutex.
                let output = self.workspaces.membership(&w).map(|(o, _)| o)
                    .or_else(|| self.space.outputs().next().cloned());
                if let Some(area) = output.and_then(|o| self.space.output_geometry(&o)) {
                    r.size.w = r.size.w.clamp(1, area.size.w.max(1));
                    r.size.h = r.size.h.clamp(1, area.size.h.max(1));
                    r.loc.x = r.loc.x.clamp(area.loc.x, area.loc.x + area.size.w - r.size.w);
                    r.loc.y = r.loc.y.clamp(area.loc.y, area.loc.y + area.size.h - r.size.h);
                }
                self.reset_tile_rect(&w, r, false, false);
            }
        }
        self.cancel_feature_grab();
    }
}
