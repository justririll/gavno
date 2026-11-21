//! Per-output dynamic workspace ownership. Only active windows enter Space.
use crate::{
    ui::spring::{Spring, SpringParams},
    Gavno,
};
use smithay::{
    desktop::{Space, Window},
    output::Output,
    utils::{IsAlive, Logical, Point},
};
use std::time::Duration;

#[derive(Debug)]
pub struct Workspace<T> {
    pub id: u64,
    /// Back to front; focusing also records the workspace's MRU.
    pub windows: Vec<T>,
    pub tiling: bool,
    pub tree: crate::tiling::Tree<Window>,
    /// Shown in the bar; see `ai::names`. `user_named` names are never replaced.
    pub name: Option<String>,
    pub user_named: bool,
    /// Window-set signature an AI-refined `name` was made for.
    pub name_sig: Option<u64>,
}
#[derive(Debug)]
pub struct WorkspaceList<T> {
    pub items: Vec<Workspace<T>>,
    pub active: usize,
    next_id: u64,
    pub(crate) default_tiling: bool,
    /// Workspaces `0..min_count` always exist; later ones stay dynamic.
    pub(crate) min_count: usize,
}
impl<T> Default for WorkspaceList<T> {
    fn default() -> Self {
        Self {
            items: vec![Workspace {
                id: 0,
                windows: Vec::new(),
                tiling: false,
                tree: Default::default(),
                name: None,
                user_named: false,
                name_sig: None,
            }],
            active: 0,
            next_id: 1,
            default_tiling: false,
            min_count: 0,
        }
    }
}
impl<T> WorkspaceList<T> {
    /// An empty list with `min_count` fixed workspaces.
    pub fn fresh(tiling: bool, min_count: usize) -> Self {
        Self::fresh_from(tiling, min_count, 0)
    }
    /// The same with workspace ids starting at `first_id`, so two worlds of
    /// one output never share an id (saved layouts are keyed by it).
    pub fn fresh_from(tiling: bool, min_count: usize, first_id: u64) -> Self {
        let mut list = Self::default();
        list.items[0].id = first_id;
        list.next_id = first_id + 1;
        list.default_tiling = tiling;
        list.min_count = min_count;
        list.items[0].tiling = tiling;
        list.append_empty();
        list
    }
    fn push_empty(&mut self) {
        self.items.push(Workspace {
            id: self.next_id,
            windows: Vec::new(),
            tiling: self.default_tiling,
            tree: Default::default(),
            name: None,
            user_named: false,
            name_sig: None,
        });
        self.next_id += 1;
    }
    /// Grows to `min_count`, then keeps one trailing empty workspace only
    /// after an occupied last one.
    pub fn append_empty(&mut self) {
        while self.items.len() < self.min_count {
            self.push_empty();
        }
        if self.items.last().is_none_or(|w| !w.windows.is_empty()) {
            self.push_empty();
        }
    }
    /// Returns indices removed, descending, so visual coordinates can be rebased.
    pub fn normalize(&mut self) -> Vec<usize> {
        let mut removed = Vec::new();
        for i in (self.min_count..self.items.len().saturating_sub(1)).rev() {
            if i != self.active && self.items[i].windows.is_empty() {
                self.items.remove(i);
                if i < self.active {
                    self.active -= 1;
                }
                removed.push(i);
            }
        }
        // With fixed workspaces the trailing empty is only needed after an occupied last one.
        while self.min_count > 0
            && self.items.len() > self.min_count
            && self.items.len() - 1 != self.active
            && self.items[self.items.len() - 1].windows.is_empty()
            && self.items[self.items.len() - 2].windows.is_empty()
        {
            self.items.pop();
            removed.push(self.items.len());
        }
        self.append_empty();
        removed
    }
    /// Creates empty workspaces until there are at least `len` (at most 64).
    pub fn grow_to(&mut self, len: usize) {
        while self.items.len() < len.min(64) {
            self.push_empty();
        }
    }
    pub fn move_item(&mut self, from: usize, index: usize, to: usize) -> bool {
        if from == to
            || from >= self.items.len()
            || to >= self.items.len()
            || index >= self.items[from].windows.len()
        {
            return false;
        }
        let item = self.items[from].windows.remove(index);
        self.items[to].windows.push(item);
        true
    }
}
#[derive(Clone, Debug)]
pub struct OwnedWindow {
    pub window: Window,
    pub location: Point<i32, Logical>,
}
pub struct OutputWorkspaces {
    pub output: Output,
    pub list: WorkspaceList<OwnedWindow>,
    /// The other world of this output: the GUI workspaces while TTY mode is
    /// on, the consoles while it is off. Its windows are never mapped.
    pub other: Option<WorkspaceList<OwnedWindow>>,
    pub position: Spring,
    pub held: bool,
}
impl OutputWorkspaces {
    pub fn append_empty(&mut self) {
        self.list.append_empty();
    }
    pub fn normalize(&mut self) {
        for index in self.list.normalize() {
            if self.position.value > index as f64 {
                self.position.value -= 1.0;
            }
            if self.position.target > index as f64 {
                self.position.target -= 1.0;
            }
        }
    }
}
#[derive(Default)]
pub struct Workspaces {
    pub outputs: Vec<OutputWorkspaces>,
    pub default_tiling: bool,
    /// Always-present workspace count, see `[workspaces] fixed`.
    pub fixed: usize,
    pub moving_tile: Option<Window>,
    pub splitter_hover: Option<(Output, crate::tiling::Splitter)>,
    pub splitter_canvas: crate::topbar::Canvas,
    /// Windows moved off an output that went away, to bring them home.
    pub exiles: Vec<Exile>,
}
/// Where a window lived before its output went away, and where it was put.
#[derive(Clone, Debug)]
pub struct Exile {
    pub window: Window,
    pub home: String,
    pub workspace: usize,
    /// Offset from the home output's origin.
    pub offset: Point<i32, Logical>,
    pub placed_on: String,
    pub placed_at: Point<i32, Logical>,
}
impl Workspaces {
    pub fn ensure(&mut self, output: &Output) -> &mut OutputWorkspaces {
        if let Some(i) = self.outputs.iter().position(|w| &w.output == output) {
            return &mut self.outputs[i];
        }
        let mut list = WorkspaceList::default();
        list.default_tiling = self.default_tiling;
        list.min_count = self.fixed;
        list.items[0].tiling = self.default_tiling;
        list.append_empty();
        self.outputs.push(OutputWorkspaces {
            output: output.clone(),
            list,
            other: None,
            position: Spring::new(0.0, SpringParams::SMOOTH),
            held: false,
        });
        self.outputs.last_mut().expect("just inserted output")
    }
    pub fn get(&self, output: &Output) -> Option<&OutputWorkspaces> {
        self.outputs.iter().find(|w| &w.output == output)
    }
    pub fn all(&self) -> impl Iterator<Item = &OwnedWindow> {
        self.outputs
            .iter()
            .flat_map(|o| &o.list.items)
            .flat_map(|w| &w.windows)
    }
    /// Windows of the hidden worlds (see `OutputWorkspaces::other`).
    pub fn stashed(&self) -> impl Iterator<Item = &OwnedWindow> {
        self.outputs
            .iter()
            .filter_map(|o| o.other.as_ref())
            .flat_map(|l| &l.items)
            .flat_map(|w| &w.windows)
    }
    pub fn is_stashed(&self, window: &Window) -> bool {
        self.stashed().any(|v| &v.window == window)
    }
    pub fn membership(&self, window: &Window) -> Option<(Output, usize)> {
        self.outputs.iter().find_map(|o| {
            o.list
                .items
                .iter()
                .position(|w| w.windows.iter().any(|v| &v.window == window))
                .map(|i| (o.output.clone(), i))
        })
    }
    pub fn location(&self, window: &Window) -> Option<Point<i32, Logical>> {
        self.all().find(|v| &v.window == window).map(|v| v.location)
    }
    pub fn active(&self) -> bool {
        self.outputs.iter().any(|o| o.position.is_active())
    }
    pub fn advance(&mut self, now: Duration) {
        for o in &mut self.outputs {
            o.position.advance(now);
        }
    }
    pub fn snapshot(&mut self, space: &Space<Window>) {
        for o in &mut self.outputs {
            if let Some(other) = &mut o.other {
                for ws in &mut other.items {
                    ws.windows.retain(|w| w.window.alive());
                }
            }
            for ws in &mut o.list.items {
                ws.windows.retain(|w| w.window.alive());
                for w in &mut ws.windows {
                    if let Some(loc) = space.element_location(&w.window) {
                        w.location = loc;
                    }
                }
            }
        }
    }
}
/// Translate saved floating geometry into the surviving work area, including hidden windows.
fn migrate_rect(
    mut rect: smithay::utils::Rectangle<i32, Logical>,
    origin: Point<i32, Logical>,
    area: smithay::utils::Rectangle<i32, Logical>,
) -> smithay::utils::Rectangle<i32, Logical> {
    rect.loc += area.loc - origin;
    rect.loc.x = rect.loc.x.clamp(area.loc.x, area.loc.x + (area.size.w - rect.size.w).max(0));
    rect.loc.y = rect.loc.y.clamp(area.loc.y, area.loc.y + (area.size.h - rect.size.h).max(0));
    rect
}
impl Gavno {
    pub fn refresh_desktop_pointer(&mut self) {
        if self.greeter.is_some() || self.protocols.locks.locked {
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();
        if pointer.is_grabbed() {
            return;
        }
        let location = pointer.current_location();
        let under = self.surface_under(location);
        if pointer.current_focus().as_ref() != under.as_ref().map(|(s, _)| s) {
            pointer.motion(
                self,
                under,
                &smithay::input::pointer::MotionEvent {
                    location,
                    serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                    time: self.clock.now().as_millis() as u32,
                },
            );
            pointer.frame(self);
        }
    }
    pub fn desktop_transform_at(&self, pos: Point<f64, Logical>) -> bool {
        self.workspaces.outputs.iter().any(|o| {
            self.space
                .output_geometry(&o.output)
                .is_some_and(|r| r.to_f64().contains(pos))
                && (o.held
                    || o.position.is_active()
                    || self.features.read(crate::module::Feature::Overview, false, || {
                        self.overview.output.as_ref() == Some(&o.output) && self.overview.visible()
                    }))
        })
    }
    pub fn refresh_workspaces(&mut self) {
        if self.greeter.is_some() {
            return;
        }
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        for output in &outputs {
            self.workspaces.ensure(output);
        }
        if self.overlay.keyboard.output().is_some_and(|output| !outputs.contains(output)) {
            self.close_keyboard_overlay();
        }
        if self.workspaces.outputs.iter().any(|o| !outputs.contains(&o.output)) {
            self.end_window_grab();
        }
        self.workspaces.snapshot(&self.space);
        self.guard(crate::module::Feature::Overview, (), |gv| {
            if gv
                .overview
                .output
                .as_ref()
                .is_some_and(|o| !outputs.contains(o))
            {
                gv.features.call(crate::module::Feature::Overview, (), || { gv.overview.output = None; });
                gv.features.call(crate::module::Feature::Overview, Default::default(), || gv.overview.progress.jump(0.0));
                gv.features.call(crate::module::Feature::Overview, (), || { gv.overview.held = false; });
                gv.features.call(crate::module::Feature::Overview, (), || { gv.overview.drag = None; });
                gv.overview.hits.clear();
            }
        });
        if outputs.is_empty() {
            return;
        }
        // Preserve windows when an output is unplugged, including inactive workspaces.
        let mut orphaned = Vec::new();
        self.workspaces.outputs.retain_mut(|o| {
            if outputs.contains(&o.output) {
                true
            } else {
                let origin = o.output.current_location();
                let home = o.output.name();
                for (index, ws) in o.list.items.iter_mut().enumerate() {
                    orphaned.extend(ws.windows.drain(..).map(|w| (w, origin, home.clone(), index)));
                }
                // The hidden world's windows must not be lost with the output.
                if let Some(other) = &mut o.other {
                    for (index, ws) in other.items.iter_mut().enumerate() {
                        orphaned.extend(ws.windows.drain(..).map(|w| (w, origin, home.clone(), index)));
                    }
                }
                false
            }
        });
        // A recreated output starts new workspace identities, even when its
        // connector name and local workspace IDs match a removed output.
        self.code_mode_saved.retain(|(output, _, _)| self.workspaces.outputs.iter().any(|owner| owner.output == *output));
        self.tty.saved.retain(|(output, _, _)| self.workspaces.outputs.iter().any(|owner| owner.output == *output));
        let mut refit = !orphaned.is_empty();
        // Orphans go to the output with the pointer, onto the same workspace.
        let target = self.active_output().filter(|o| outputs.contains(o)).or_else(|| outputs.first().cloned());
        if let Some(output) = target.as_ref() {
            for (mut w, origin, home, index) in orphaned {
                self.space.unmap_elem(&w.window);
                let offset = w.location - origin;
                let area = self.work_area(output);
                let size = w.window.toplevel().and_then(|t| t.with_pending_state(|s| s.size))
                    .unwrap_or(w.window.geometry().size);
                w.location = migrate_rect(smithay::utils::Rectangle::new(w.location, size), origin, area).loc;
                crate::window_mode::with_mode_state(&w.window, |s| {
                    s.restore = s.restore.map(|r| migrate_rect(r, origin, area));
                    s.tile_restore = s.tile_restore.map(|r| migrate_rect(r, origin, area));
                });
                crate::grabs::resize_grab::clear_anchor(&w.window);
                self.features.call(crate::module::Feature::Animations, (), || self.window_animations.cancel(&w.window));
                self.features.call(crate::module::Feature::Dock, (), || self.dock.cancel_window_motion(&w.window));
                let owner = self.workspaces.ensure(output);
                owner.list.grow_to(index + 1);
                let index = index.min(owner.list.items.len() - 1);
                let shown = index == owner.list.active;
                self.workspaces.exiles.retain(|e| e.window != w.window);
                self.workspaces.exiles.push(Exile {
                    window: w.window.clone(), home, workspace: index, offset,
                    placed_on: output.name(), placed_at: w.location,
                });
                if shown && !crate::window_mode::mode_state(&w.window).minimized { self.space.map_element(w.window.clone(), w.location, false); }
                let owner = self.workspaces.ensure(output);
                owner.list.items[index].windows.push(w);
            }
        }
        refit |= self.bring_exiles_home(&outputs);
        // Windows mapped while no output existed never got an owner.
        if let Some(output) = outputs.first() {
            let homeless: Vec<_> = self
                .space
                .elements()
                .filter(|w| self.workspaces.membership(w).is_none())
                .map(|w| (w.clone(), self.space.element_location(w).unwrap_or_default()))
                .collect();
            for (window, location) in homeless {
                let area = self.work_area(output);
                let size = window.geometry().size;
                let location = Point::from((
                    location.x.clamp(area.loc.x, (area.loc.x + area.size.w - size.w).max(area.loc.x)),
                    location.y.clamp(area.loc.y, (area.loc.y + area.size.h - size.h).max(area.loc.y)),
                ));
                self.space.map_element(window.clone(), location, false);
                let owner = self.workspaces.ensure(output);
                let active = owner.list.active;
                owner.list.items[active].windows.push(OwnedWindow { window, location });
            }
        }
        // Fixed-size windows moved off a removed output must fit the
        // surviving one, including hidden and minimized windows.
        if refit {
            self.loop_handle.insert_idle(|gv| gv.refit_fixed_windows());
        }
        // A dragged window changes output ownership once its centre crosses outputs.
        let transfers: Vec<_> = self
            .space
            .elements()
            .filter_map(|w| {
                if self.tiled(w) { return None; }
                let (old, _) = self.workspaces.membership(w)?;
                let loc = self.space.element_location(w)?;
                let size = w.geometry().size;
                let centre = loc + Point::from((size.w / 2, size.h / 2));
                let new = self
                    .space
                    .outputs()
                    .find(|o| {
                        self.space
                            .output_geometry(o)
                            .is_some_and(|r| r.contains(centre))
                    })
                    .cloned()?;
                (old != new).then(|| (w.clone(), new))
            })
            .collect();
        for (w, output) in transfers {
            for o in &mut self.workspaces.outputs {
                for ws in &mut o.list.items {
                    ws.windows.retain(|v| v.window != w);
                }
            }
            let loc = self.space.element_location(&w).unwrap_or_default();
            let owner = self.workspaces.ensure(&output);
            owner.list.items[owner.list.active]
                .windows
                .push(OwnedWindow {
                    window: w,
                    location: loc,
                });
        }
        let mut changed = false;
        for o in &mut self.workspaces.outputs {
            if !o.held && !o.position.is_active() {
                let before = o.list.items.len();
                o.normalize();
                changed |= before != o.list.items.len();
            }
        }
        if changed {
            self.queue_redraw_all();
        }
        self.reflow_tiling(true);
    }
    /// Moves the pointer to `location` and updates what it hovers.
    pub fn warp_pointer(&mut self, location: Point<f64, Logical>) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        if pointer.is_grabbed() {
            return;
        }
        let under = self.surface_under(location);
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location,
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: self.clock.now().as_millis() as u32,
            },
        );
        pointer.frame(self);
    }
    /// Outputs left to right, then top to bottom.
    pub fn outputs_in_order(&self) -> Vec<Output> {
        let mut outputs: Vec<Output> = self.space.outputs().cloned().collect();
        outputs.sort_by_key(|o| self.space.output_geometry(o).map(|r| (r.loc.x, r.loc.y)));
        outputs
    }
    fn output_step(&self, from: &Output, next: bool) -> Option<Output> {
        let outputs = self.outputs_in_order();
        if outputs.len() < 2 {
            return None;
        }
        let i = outputs.iter().position(|o| o == from)?;
        let j = if next { (i + 1) % outputs.len() } else { (i + outputs.len() - 1) % outputs.len() };
        Some(outputs[j].clone())
    }
    /// Mod+. / Mod+, : puts the pointer in the middle of the neighbouring
    /// output and focuses its topmost window there.
    pub fn focus_output_step(&mut self, next: bool) {
        let Some(from) = self.active_output() else { return };
        let Some(target) = self.output_step(&from, next) else { return };
        let Some(geometry) = self.space.output_geometry(&target) else { return };
        let centre = geometry.loc.to_f64() + geometry.size.to_f64().downscale(2.0).to_point();
        self.warp_pointer(centre);
        let top = self.workspaces.get(&target).and_then(|o| {
            o.list.items[o.list.active].windows.iter().rev()
                .find(|w| !crate::window_mode::mode_state(&w.window).minimized)
                .map(|w| w.window.clone())
        });
        self.focus_window(top.as_ref());
        self.queue_redraw_all();
    }
    /// Mod+Shift+. / Mod+Shift+, : sends the focused window to the
    /// neighbouring output, and the pointer with it.
    pub fn move_window_output_step(&mut self, next: bool) {
        let Some(window) = self.focused_window() else { return };
        let Some(from) = self.output_for_window(&window) else { return };
        let Some(target) = self.output_step(&from, next) else { return };
        if self.move_window_to_output(&window, &target, None, None) {
            if let Some(rect) = self.space.element_geometry(&window) {
                self.warp_pointer(rect.loc.to_f64() + rect.size.to_f64().downscale(2.0).to_point());
            }
        }
    }
    /// Returns windows to an output that came back, unless the user moved
    /// them since. True when anything moved.
    fn bring_exiles_home(&mut self, outputs: &[Output]) -> bool {
        if self.workspaces.exiles.is_empty() {
            return false;
        }
        let mut moved = false;
        let mut keep = Vec::new();
        for exile in std::mem::take(&mut self.workspaces.exiles) {
            if !exile.window.alive() {
                continue;
            }
            let Some(home) = outputs.iter().find(|o| o.name() == exile.home).cloned() else {
                keep.push(exile);
                continue;
            };
            let untouched = self.workspaces.membership(&exile.window).is_some_and(|(o, _)| o.name() == exile.placed_on)
                && (self.tiled(&exile.window) || self.workspaces.location(&exile.window) == Some(exile.placed_at));
            if untouched {
                let location = home.current_location() + exile.offset;
                moved |= self.move_window_to_output(&exile.window, &home, Some(exile.workspace), Some(location));
            }
        }
        self.workspaces.exiles = keep;
        moved
    }
    /// Moves `window` to `target`, onto `workspace` (default: the active
    /// one) at `location` (default: the same place relative to the output),
    /// keeping its size and window mode.
    pub fn move_window_to_output(
        &mut self,
        window: &Window,
        target: &Output,
        workspace: Option<usize>,
        location: Option<Point<i32, Logical>>,
    ) -> bool {
        self.workspaces.snapshot(&self.space);
        let Some((from, from_index)) = self.workspaces.membership(window) else {
            return false;
        };
        if &from == target && workspace.is_none_or(|w| w == from_index) {
            return false;
        }
        self.end_window_grab();
        let was_focused = self.focused_window().as_ref() == Some(window);
        let origin = from.current_location();
        let mut owned = None;
        for o in &mut self.workspaces.outputs {
            for ws in &mut o.list.items {
                if let Some(i) = ws.windows.iter().position(|v| &v.window == window) {
                    owned = Some(ws.windows.remove(i));
                }
                self.features.call(crate::module::Feature::Tiling, (), || ws.tree.remove(window));
            }
        }
        let Some(mut owned) = owned else {
            return false;
        };
        window.set_activated(false);
        self.space.unmap_elem(window);
        let area = self.work_area(target);
        let target_origin = target.current_location();
        let size = window.toplevel().and_then(|t| t.with_pending_state(|s| s.size)).unwrap_or(window.geometry().size);
        let wanted = location.unwrap_or(owned.location - origin + target_origin);
        owned.location = migrate_rect(smithay::utils::Rectangle::new(wanted, size), area.loc, area).loc;
        crate::window_mode::with_mode_state(window, |s| {
            s.restore = s.restore.map(|r| migrate_rect(r, origin, area));
            s.tile_restore = s.tile_restore.map(|r| migrate_rect(r, origin, area));
        });
        crate::grabs::resize_grab::clear_anchor(window);
        self.features.call(crate::module::Feature::Animations, (), || self.window_animations.cancel(window));
        let owner = self.workspaces.ensure(target);
        let index = workspace.unwrap_or(owner.list.active);
        owner.list.grow_to(index + 1);
        let index = index.min(owner.list.items.len() - 1);
        let shown = index == owner.list.active;
        let at = owned.location;
        owner.list.items[index].windows.push(owned);
        owner.append_empty();
        let mode = crate::window_mode::mode_state(window);
        if shown && !mode.minimized {
            self.space.map_element(window.clone(), at, false);
            if mode.mode != crate::window_mode::WindowMode::Floating {
                self.set_window_mode(window, mode.mode, Some(target.clone()));
            }
        }
        if let Some(t) = window.toplevel() {
            t.send_pending_configure();
        }
        self.reflow_tiling(true);
        self.clear_desktop_pointer();
        if was_focused {
            if shown { self.focus_window(Some(window)); } else { self.focus_topmost(); }
        }
        self.queue_redraw_all();
        true
    }
    pub fn save_workspace_location(&mut self, window: &Window, location: Point<i32, Logical>) {
        for o in &mut self.workspaces.outputs {
            for ws in &mut o.list.items {
                if let Some(v) = ws.windows.iter_mut().find(|v| &v.window == window) {
                    v.location = location;
                }
            }
        }
    }
    pub fn workspace_forget(&mut self, window: &Window) {
        self.cancel_window_gestures();
        self.guard(crate::module::Feature::Overview, (), |gv| gv.overview.forget(window));
        for o in &mut self.workspaces.outputs {
            for ws in o.list.items.iter_mut().chain(o.other.iter_mut().flat_map(|l| l.items.iter_mut())) {
                ws.windows.retain(|w| &w.window != window);
                self.features.call(crate::module::Feature::Tiling, (), || ws.tree.remove(window));
            }
        }
        self.reflow_tiling(true);
    }
    pub fn workspace_focus(&mut self, window: &Window) {
        for o in &mut self.workspaces.outputs {
            for ws in &mut o.list.items {
                if let Some(i) = ws.windows.iter().position(|v| &v.window == window) {
                    let w = ws.windows.remove(i);
                    ws.windows.push(w);
                }
            }
        }
    }
    pub fn switch_workspace(&mut self, output: &Output, index: usize) -> bool {
        self.workspaces.snapshot(&self.space);
        let now = self.start_time.elapsed();
        let o = self.workspaces.ensure(output);
        if index >= o.list.items.len() {
            return false;
        }
        if index == o.list.active
            && !o.held
            && !o.position.is_active()
            && (o.position.value - index as f64).abs() < 0.0005
        {
            return true;
        }
        self.end_window_grab();
        let o = self.workspaces.ensure(output);
        o.position.advance(now);
        o.position.set(index as f64);
        o.held = false;
        let old = o.list.active;
        for w in &o.list.items[old].windows {
            w.window.set_activated(false);
            if let Some(t) = w.window.toplevel() {
                t.send_pending_configure();
            }
            self.space.unmap_elem(&w.window);
        }
        o.list.active = index;
        for w in &o.list.items[index].windows {
            if !crate::window_mode::mode_state(&w.window).minimized { self.space.map_element(w.window.clone(), w.location, false); }
        }
        let focus = o.list.items[index].windows.iter().rev().find(|w| !crate::window_mode::mode_state(&w.window).minimized).map(|w| w.window.clone());
        self.mru = self
            .workspaces
            .outputs
            .iter()
            .flat_map(|o| {
                o.list.items[o.list.active]
                    .windows
                    .iter()
                    .rev()
                    .map(|v| v.window.clone())
            })
            .collect();
        self.reflow_tiling(true);
        self.clear_desktop_pointer();
        self.focus_window(focus.as_ref());
        if self.tty.on {
            // Consoles do not slide: jump there and dip through black.
            self.workspaces.ensure(output).position.jump(index as f64);
            self.tty_console_switched();
        }
        self.queue_redraw_all();
        true
    }
    pub fn workspace_step(&mut self, backwards: bool) {
        if let Some(output) = self.active_output() {
            let o = self.workspaces.ensure(&output);
            let next = if backwards {
                o.list.active.saturating_sub(1)
            } else {
                (o.list.active + 1).min(o.list.items.len() - 1)
            };
            self.switch_workspace(&output, next);
        }
    }
    pub fn move_to_workspace(&mut self, window: &Window, index: usize) -> bool {
        let was_focused = self.focused_window().as_ref() == Some(window);
        self.workspaces.snapshot(&self.space);
        let Some((output, from)) = self.workspaces.membership(window) else {
            return false;
        };
        let o = self.workspaces.ensure(&output);
        let Some(i) = o.list.items[from]
            .windows
            .iter()
            .position(|v| &v.window == window)
        else {
            return false;
        };
        if index >= o.list.items.len() || index == from {
            return index == from;
        }
        self.end_window_grab();
        let o = self.workspaces.ensure(&output);
        if !o.list.move_item(from, i, index) {
            return index == from;
        }
        if index != o.list.active {
            window.set_activated(false);
            if let Some(t) = window.toplevel() {
                t.send_pending_configure();
            }
            self.space.unmap_elem(window);
        } else {
            let loc = o.list.items[index]
                .windows
                .last()
                .map(|v| v.location)
                .unwrap_or_default();
            if !crate::window_mode::mode_state(window).minimized { self.space.map_element(window.clone(), loc, false); }
        }
        // Append the new trailing empty immediately; removal waits until motion settles.
        o.append_empty();
        self.reflow_tiling(true);
        self.clear_desktop_pointer();
        if was_focused {
            self.focus_topmost();
        }
        self.queue_redraw_all();
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trailing_empty_and_active_exception() {
        let mut l = WorkspaceList::<u32>::default();
        l.items[0].windows.push(1);
        l.normalize();
        assert_eq!(l.items.len(), 2);
        l.active = 1;
        l.items[0].windows.clear();
        l.normalize();
        assert_eq!(l.active, 0);
        assert_eq!(l.items.len(), 1);
    }
    #[test]
    fn move_and_stable_identity() {
        let mut l = WorkspaceList::default();
        l.items[0].windows.push(7);
        l.normalize();
        let id = l.items[1].id;
        assert!(l.move_item(0, 0, 1));
        l.active = 1;
        l.normalize();
        assert_eq!(l.active, 0);
        assert_eq!(l.items[0].id, id);
        assert_eq!(l.items[0].windows, vec![7]);
        assert!(l.items[1].windows.is_empty());
        assert!(!l.move_item(0, 0, 9));
    }
    #[test]
    fn active_empty_is_kept_until_leaving() {
        let mut l = WorkspaceList::default();
        l.items[0].windows.push(1);
        l.normalize();
        l.items[1].windows.push(2);
        l.normalize();
        l.items[0].windows.clear();
        l.normalize();
        assert_eq!(l.items.len(), 3);
        l.active = 1;
        let id = l.items[1].id;
        l.normalize();
        assert_eq!(l.active, 0);
        assert_eq!(l.items[0].id, id);
        assert_eq!(l.items.len(), 2);
    }
    fn fixed_list(min: usize) -> WorkspaceList<u32> {
        let mut l = WorkspaceList::<u32>::default();
        l.min_count = min;
        l.normalize();
        l
    }
    #[test]
    fn normalize_grows_to_min_count_and_never_removes_fixed_ones() {
        let mut l = fixed_list(10);
        assert_eq!(l.items.len(), 10);
        let ids: Vec<_> = l.items.iter().map(|w| w.id).collect();
        l.active = 6;
        assert!(l.normalize().is_empty());
        assert_eq!(l.items.len(), 10);
        l.active = 0;
        l.items[3].windows.push(1);
        assert!(l.normalize().is_empty());
        assert_eq!(l.items.iter().map(|w| w.id).collect::<Vec<_>>(), ids, "ids stay stable");
    }
    #[test]
    fn dynamic_workspace_appears_only_after_the_last_fixed_one_is_used() {
        let mut l = fixed_list(10);
        l.items[9].windows.push(1);
        l.normalize();
        assert_eq!(l.items.len(), 11);
        // An extra empty one beyond 11 is removed again, and so is 11 once 10 empties.
        l.items[10].windows.push(2);
        l.normalize();
        assert_eq!(l.items.len(), 12);
        l.items[10].windows.clear();
        let removed = l.normalize();
        assert_eq!(l.items.len(), 11);
        assert_eq!(removed, [10]);
        l.items[9].windows.clear();
        let removed = l.normalize();
        assert_eq!(l.items.len(), 10);
        assert_eq!(removed, [10]);
        // The active empty extra workspace survives until left.
        l.items[9].windows.push(3);
        l.normalize();
        l.active = 10;
        l.items[9].windows.clear();
        l.normalize();
        assert_eq!(l.items.len(), 11);
        l.active = 9;
        l.normalize();
        assert_eq!(l.items.len(), 10);
    }
    #[test]
    fn zero_min_count_is_the_old_dynamic_list() {
        let mut l = fixed_list(0);
        assert_eq!(l.items.len(), 1);
        l.items[0].windows.push(1);
        l.normalize();
        assert_eq!(l.items.len(), 2);
        l.items[0].windows.clear();
        l.active = 1;
        assert_eq!(l.normalize(), [0]);
        assert_eq!(l.items.len(), 1);
    }
    fn output(gv: &mut Gavno, name: &str, x: i32) -> Output {
        let o = Output::new(
            name.into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        o.change_current_state(
            Some(smithay::output::Mode {
                size: (1600, 900).into(),
                refresh: 120_000,
            }),
            None,
            None,
            Some((x, 0).into()),
        );
        gv.space.map_output(&o, (x, 0));
        gv.workspaces.ensure(&o);
        o
    }
    #[test]
    fn switching_unmaps_inactive_windows_and_keeps_geometry_modes_and_mru() {
        let mut h = crate::state::tests::Harness::with_fixed_workspaces(0);
        let o = output(&mut h.gv, "left", 0);
        let first = h.window();
        h.gv.map_new_window(first.clone());
        let second = h.window();
        h.gv.map_new_window(second.clone());
        h.gv.space.map_element(first.clone(), (123, 234), false);
        crate::window_mode::with_mode_state(&first, |s| {
            s.enter(
                crate::window_mode::WindowMode::Maximized,
                Some(smithay::utils::Rectangle::new(
                    (11, 22).into(),
                    (300, 200).into(),
                )),
            )
        });
        h.gv.focus_window(Some(&first));
        assert!(h.gv.switch_workspace(&o, 1));
        assert_eq!(h.gv.space.elements().count(), 0);
        assert_eq!(h.gv.workspaces.membership(&first), Some((o.clone(), 0)));
        assert!(h
            .gv
            .window_for_surface(first.toplevel().unwrap().wl_surface())
            .is_some());
        assert!(h.gv.switch_workspace(&o, 0));
        assert_eq!(h.gv.space.element_location(&first), Some((123, 234).into()));
        assert_eq!(h.gv.focused_window(), Some(first.clone()));
        assert_eq!(
            crate::window_mode::mode_state(&first).mode,
            crate::window_mode::WindowMode::Maximized
        );
        assert!(h.gv.move_to_workspace(&first, 1));
        assert!(h.gv.space.element_location(&first).is_none());
        assert_eq!(h.gv.workspaces.get(&o).unwrap().list.items.len(), 3);
        assert!(!h.gv.move_to_workspace(&second, 99));
    }
    #[test]
    fn outputs_switch_independently_and_removal_preserves_inactive_windows() {
        let mut h = crate::state::tests::Harness::new();
        let left = output(&mut h.gv, "left", 0);
        let right = output(&mut h.gv, "right", 1600);
        let a = h.window();
        h.gv.map_new_window(a.clone());
        h.gv.space.map_element(a.clone(), (1800, 200), false);
        h.gv.refresh_workspaces();
        assert_eq!(h.gv.workspaces.membership(&a), Some((right.clone(), 0)));
        h.gv.switch_workspace(&right, 1);
        assert!(h.gv.space.element_location(&a).is_none());
        assert_eq!(h.gv.workspaces.get(&left).unwrap().list.active, 0);
        h.gv.space.unmap_output(&right);
        h.gv.refresh_workspaces();
        assert_eq!(h.gv.workspaces.membership(&a), Some((left, 0)));
        assert!(h.gv.space.element_location(&a).is_some());
        let remaining = h.gv.space.outputs().next().cloned().unwrap();
        h.gv.space.unmap_output(&remaining);
        h.gv.refresh_workspaces();
        assert!(h.gv.workspaces.membership(&a).is_some());
    }
    #[test]
    fn windows_return_to_an_output_that_comes_back_unless_moved() {
        let mut h = crate::state::tests::Harness::new();
        let left = output(&mut h.gv, "left", 0);
        let right = output(&mut h.gv, "right", 1600);
        let stays = h.window();
        let moved = h.window();
        for w in [&stays, &moved] {
            h.gv.map_new_window(w.clone());
            w.toplevel().unwrap().with_pending_state(|s| s.size = Some((400, 300).into()));
            h.gv.space.map_element(w.clone(), (1800, 200), false);
        }
        h.gv.refresh_workspaces();
        h.gv.move_to_workspace(&stays, 2);
        assert_eq!(h.gv.workspaces.membership(&stays), Some((right.clone(), 2)));
        h.gv.space.unmap_output(&right);
        h.gv.refresh_workspaces();
        // Same workspace number on the surviving output, still hidden there.
        assert_eq!(h.gv.workspaces.membership(&stays), Some((left.clone(), 2)));
        assert!(h.gv.space.element_location(&stays).is_none());
        assert_eq!(h.gv.workspaces.membership(&moved), Some((left.clone(), 0)));
        // The user moves one of them; only the other comes home.
        h.gv.space.map_element(moved.clone(), (300, 400), false);
        h.gv.refresh_workspaces();
        let back = output(&mut h.gv, "right", 1600);
        h.gv.refresh_workspaces();
        assert_eq!(h.gv.workspaces.membership(&stays), Some((back.clone(), 2)));
        assert_eq!(h.gv.workspaces.location(&stays).map(|p| p.x), Some(1800));
        assert_eq!(h.gv.workspaces.membership(&moved), Some((left.clone(), 0)));
        assert!(h.gv.workspaces.exiles.is_empty());
    }
    #[test]
    fn mod_period_moves_the_focused_window_to_the_next_output() {
        let mut h = crate::state::tests::Harness::new();
        let left = output(&mut h.gv, "left", 0);
        let right = output(&mut h.gv, "right", 1600);
        let w = h.window();
        h.gv.map_new_window(w.clone());
        w.toplevel().unwrap().with_pending_state(|s| s.size = Some((400, 300).into()));
        h.gv.space.map_element(w.clone(), (100, 200), false);
        h.gv.refresh_workspaces();
        h.gv.focus_window(Some(&w));
        assert_eq!(h.gv.workspaces.membership(&w), Some((left.clone(), 0)));
        h.gv.move_window_output_step(true);
        assert_eq!(h.gv.workspaces.membership(&w), Some((right.clone(), 0)));
        assert_eq!(h.gv.space.element_location(&w).map(|p| p.x), Some(1700));
        h.gv.move_window_output_step(true);
        assert_eq!(h.gv.workspaces.membership(&w), Some((left, 0)));
    }
    #[test]
    fn output_removal_migrates_tile_and_mode_restores_including_minimized_windows() {
        use crate::window_mode::{mode_state, with_mode_state};
        use smithay::utils::Rectangle;
        let mut h = crate::state::tests::Harness::new();
        let left = output(&mut h.gv, "left", 0);
        let right = output(&mut h.gv, "right", 1600);
        let mut windows = Vec::new();
        for x in [1800, 2900] {
            let w = h.window();
            h.gv.map_new_window(w.clone());
            w.toplevel().unwrap().with_pending_state(|s| s.size = Some((500, 400).into()));
            h.gv.space.map_element(w.clone(), (x, 200), false);
            windows.push(w);
        }
        h.gv.refresh_workspaces();
        h.gv.toggle_workspace_tiling(&right);
        h.gv.minimize_window(&windows[1]);
        let saved: Vec<_> = windows.iter().map(|w| mode_state(w).tile_restore.unwrap()).collect();
        for w in &windows {
            with_mode_state(w, |s| s.restore = Some(Rectangle::new((3100, 800).into(), (500, 400).into())));
        }
        // Include an inactive source workspace as well as a minimized window.
        h.gv.switch_workspace(&right, 1);
        h.gv.space.unmap_output(&right);
        h.gv.refresh_workspaces();
        let area = h.gv.work_area(&left);
        for (w, saved) in windows.iter().zip(saved) {
            let expected = migrate_rect(saved, (1600, 0).into(), area);
            assert_eq!(h.gv.workspaces.membership(w), Some((left.clone(), 0)));
            assert_eq!(h.gv.workspaces.location(w), Some(expected.loc));
            assert_eq!(w.toplevel().unwrap().with_pending_state(|s| s.size), Some(expected.size));
            assert_eq!(mode_state(w).tile_restore, None);
            assert!(area.contains(mode_state(w).restore.unwrap().loc));
        }
        assert!(h.gv.space.element_location(&windows[1]).is_none());
        h.gv.focus_window(Some(&windows[1]));
        assert!(area.contains(h.gv.space.element_location(&windows[1]).unwrap()));
    }
    #[test]
    fn strip_retargets_without_jumping_and_settles_to_idle() {
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h.gv, "test", 0);
        let w = h.window();
        h.gv.map_new_window(w);
        h.gv.switch_workspace(&o, 1);
        h.gv.workspaces.advance(Duration::from_millis(60));
        let before = h.gv.workspaces.get(&o).unwrap().position.value;
        // Retarget directly at the same monotonic instant, as a rapid key repeat does.
        h.gv.workspaces.ensure(&o).position.set(0.0);
        assert_eq!(before, h.gv.workspaces.get(&o).unwrap().position.value);
        for i in 1..500 {
            h.gv.workspaces.advance(Duration::from_millis(60 + i * 8));
        }
        assert!(!h.gv.workspaces.active());
        assert_eq!(h.gv.workspaces.get(&o).unwrap().position.value, 0.0);
    }
    #[test]
    fn overview_and_caught_strip_stop_requesting_frames() {
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h.gv, "test", 0);
        let w = h.window();
        h.gv.map_new_window(w);
        h.gv.window_animations.entries.clear();
        h.gv.overview.set(o.clone(), true, Duration::ZERO);
        for i in 1..500 {
            h.gv.overview.scene(
                &mut h.gv.workspaces,
                &h.gv.space,
                &o,
                Duration::from_millis(i * 8),
            );
        }
        assert!(h.gv.overview.open());
        assert!(!h.gv.is_animating());
        let owner = h.gv.workspaces.ensure(&o);
        owner.position.jump(0.4);
        owner.held = true;
        h.gv.overview.progress.jump(0.5);
        h.gv.overview.held = true;
        assert!(!h.gv.is_animating());
        let first =
            h.gv.overview
                .scene(
                    &mut h.gv.workspaces,
                    &h.gv.space,
                    &o,
                    Duration::from_secs(10),
                )
                .unwrap()[0]
                .rect;
        let second =
            h.gv.overview
                .scene(
                    &mut h.gv.workspaces,
                    &h.gv.space,
                    &o,
                    Duration::from_secs(20),
                )
                .unwrap()[0]
                .rect;
        assert_eq!(first, second);
        h.gv.overview.set(o.clone(), false, Duration::from_secs(20));
        h.gv.workspaces.ensure(&o).position.set(0.0);
        h.gv.workspaces.ensure(&o).held = false;
        for i in 1..500 {
            h.gv.overview.scene(
                &mut h.gv.workspaces,
                &h.gv.space,
                &o,
                Duration::from_millis(20_000 + i * 8),
            );
        }
        assert!(!h.gv.is_animating());
        assert!(!h.gv.overview.visible());
    }
    #[test]
    fn held_swipes_advance_in_the_scene_and_return_to_idle_without_release() {
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h.gv, "test", 0);
        let w = h.window();
        h.gv.map_new_window(w);
        h.gv.window_animations.entries.clear();
        let owner = h.gv.workspaces.ensure(&o);
        owner.held = true;
        owner.position.follow(0.4, Duration::ZERO);
        h.gv.overview.output = Some(o.clone());
        h.gv.overview.held = true;
        h.gv.overview.progress.follow(0.6, Duration::ZERO);
        assert!(h.gv.is_animating());
        let mut previous = (0.0, 0.0);
        for frame in 1..=12 {
            h.gv.overview.scene(
                &mut h.gv.workspaces,
                &h.gv.space,
                &o,
                Duration::from_micros(frame * 8_333),
            );
            let current = (
                h.gv.workspaces.get(&o).unwrap().position.value,
                h.gv.overview.progress.value,
            );
            assert!(current.0 >= previous.0 && current.1 >= previous.1);
            if frame <= 2 {
                assert!(current.0 > previous.0 && current.1 > previous.1);
                assert!(
                    h.gv.is_animating(),
                    "request the next vblank without new input"
                );
            }
            previous = current;
        }
        assert_eq!(previous, (0.4, 0.6));
        assert!(h.gv.workspaces.get(&o).unwrap().held);
        assert!(h.gv.overview.held);
        assert!(
            !h.gv.is_animating(),
            "stationary fingers must let the backend idle"
        );
        let mut stationary = Vec::new();
        for seconds in [1, 2] {
            stationary.push(
                h.gv.overview
                    .scene(
                        &mut h.gv.workspaces,
                        &h.gv.space,
                        &o,
                        Duration::from_secs(seconds),
                    )
                    .unwrap()[0]
                    .rect,
            );
        }
        assert_eq!(stationary[0], stationary[1]);
        assert!(!h.gv.is_animating());
    }
    #[test]
    fn hidden_mode_changes_keep_output_and_floating_restore_geometry() {
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h.gv, "test", 0);
        let w = h.window();
        h.gv.map_new_window(w.clone());
        h.gv.space.map_element(w.clone(), (123, 234), false);
        h.gv.switch_workspace(&o, 1);
        h.gv.set_window_mode(&w, crate::window_mode::WindowMode::Maximized, None);
        let saved = crate::window_mode::mode_state(&w);
        assert_eq!(saved.restore.unwrap().loc, (123, 234).into());
        assert_eq!(
            h.gv.workspaces.location(&w),
            Some(h.gv.work_area(&o).loc + Point::from((h.gv.config.gap, h.gv.config.gap)))
        );
        assert!(h.gv.space.element_location(&w).is_none());
        h.gv.set_window_mode(&w, crate::window_mode::WindowMode::Floating, None);
        assert_eq!(h.gv.workspaces.location(&w), Some((123, 234).into()));
        h.gv.switch_workspace(&o, 0);
        assert_eq!(h.gv.space.element_location(&w), Some((123, 234).into()));
    }
    #[test]
    fn fresh_output_has_ten_workspaces_and_reaches_seven_and_ten() {
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h.gv, "left", 0);
        assert_eq!(h.gv.workspaces.get(&o).unwrap().list.items.len(), 10);
        assert!(h.gv.switch_workspace(&o, 6));
        assert_eq!(h.gv.workspaces.get(&o).unwrap().list.active, 6);
        assert!(!h.gv.switch_workspace(&o, 10), "no 11th workspace until 10 is used");
        assert!(h.gv.switch_workspace(&o, 0));
        let w = h.window();
        h.gv.map_new_window(w.clone());
        assert!(h.gv.move_to_workspace(&w, 9));
        assert_eq!(h.gv.workspaces.membership(&w), Some((o.clone(), 9)));
        assert_eq!(h.gv.workspaces.get(&o).unwrap().list.items.len(), 11, "using 10 appends 11");
        h.gv.workspace_step(false);
        assert_eq!(h.gv.workspaces.get(&o).unwrap().list.active, 1);
    }
    #[test]
    fn workspace_step_stops_at_the_last_fixed_workspace() {
        let mut h = crate::state::tests::Harness::new();
        let o = output(&mut h.gv, "left", 0);
        h.gv.workspaces.ensure(&o).list.active = 9;
        h.gv.switch_workspace(&o, 9);
        h.gv.workspace_step(false);
        assert_eq!(h.gv.workspaces.get(&o).unwrap().list.active, 9);
    }
}

/// Core workspace strips remain available when Overview is disabled.
pub(crate) fn workspace_scene(
    workspaces: &mut Workspaces, space: &Space<Window>, output: &Output, now: Duration,
) -> Option<Vec<crate::overview::SceneWindow>> {
    workspaces.advance(now);
    let owner = workspaces.get(output)?;
    if !owner.held && !owner.position.is_active()
        && (owner.position.value - owner.list.active as f64).abs() < 0.0005 { return None; }
    let geo = space.output_geometry(output)?;
    let mut scene = Vec::new();
    for (index, ws) in owner.list.items.iter().enumerate() {
        for v in &ws.windows {
            if crate::window_mode::mode_state(&v.window).minimized { continue; }
            let location = space.element_location(&v.window).unwrap_or(v.location);
            let offset = Point::from(((index as f64 - owner.position.value) * geo.size.w as f64, 0.0));
            scene.push(crate::overview::SceneWindow {
                window: v.window.clone(), location,
                rect: smithay::utils::Rectangle::new(location.to_f64() - geo.loc.to_f64() + offset,
                    v.window.geometry().size.to_f64()),
            });
        }
    }
    Some(scene)
}
