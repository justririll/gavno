//! Suspended tiling layouts and the bounded, event-driven Classic window store.
use super::ShellMode;
use crate::{
    tiling::{Rect, Tree},
    window_mode::{configure_mode, mode_state, with_mode_state, WindowMode},
    Gavno,
};
use serde::{Deserialize, Serialize};
use smithay::{
    desktop::Window,
    output::Output,
    utils::IsAlive,
    wayland::{
        compositor::with_states,
        shell::xdg::{SurfaceCachedState, XdgToplevelSurfaceData},
    },
};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
};

const LIMIT: usize = 128;

struct Layout {
    output: Output,
    workspace: u64,
    tiling: bool,
    tree: Tree<Window>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    app_id: String,
    output: String,
    /// None means maximized; floating geometry is relative to the work area.
    geometry: Option<[i32; 4]>,
}

struct Writer {
    pending: Arc<Mutex<Option<Vec<Entry>>>>,
    wake: mpsc::SyncSender<()>,
}
impl Writer {
    fn submit(&self, entries: Vec<Entry>) -> bool {
        // Neither side holds this lock during I/O. Keep only the newest
        // snapshot even when a slow disk cannot keep up with close bursts.
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(entries);
        !matches!(
            self.wake.try_send(()),
            Err(mpsc::TrySendError::Disconnected(()))
        )
    }
}

pub(crate) struct Windows {
    active: bool,
    layouts: Vec<Layout>,
    /// Tiles and new auto-maximized windows return to tiling when appropriate.
    tiles: Vec<Window>,
    entries: Vec<Entry>,
    loaded: bool,
    dirty: bool,
    writer: Option<Writer>,
    waiting: Vec<Window>,
    /// The store did not answer in time: windows are no longer held back.
    gave_up: bool,
    /// Code mode was on when Classic was entered; returning turns it back on.
    code_resume: bool,
    code_saved: Vec<(Output, u64, bool)>,
}
impl Windows {
    /// Whether Classic currently owns the window layout.
    pub fn in_classic(&self) -> bool {
        self.active
    }
    /// Records whether code mode should come back when Classic ends.
    pub fn set_code_resume(&mut self, resume: bool) {
        self.code_resume = resume;
    }
    pub fn new(active: bool) -> Self {
        Self {
            active,
            layouts: Vec::new(),
            tiles: Vec::new(),
            entries: Vec::new(),
            loaded: false,
            dirty: false,
            writer: None,
            waiting: Vec::new(),
            gave_up: false,
            code_resume: false,
            code_saved: Vec::new(),
        }
    }
}

/// xdg-shell has no utility/splash type. Parented and fixed-size surfaces are
/// the protocol signals available here; layer-shell surfaces never reach this path.
pub(crate) fn normal_window(window: &Window) -> bool {
    let Some(t) = window.toplevel() else {
        return false;
    };
    t.parent().is_none()
        && with_states(t.wl_surface(), |states| {
            let mut cached = states.cached_state.get::<SurfaceCachedState>();
            let c = cached.current();
            !(c.min_size.w > 0 && c.min_size.h > 0 && c.min_size == c.max_size)
        })
}

fn app_id(window: &Window) -> Option<String> {
    let t = window.toplevel()?;
    with_states(t.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()?
            .lock()
            .ok()?
            .app_id
            .clone()
    })
    .filter(|id| !id.is_empty())
}

fn state_path(state_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    state_home
        .filter(|p| p.is_absolute())
        .or_else(|| {
            home.filter(|p| p.is_absolute())
                .map(|p| p.join(".local/state"))
        })
        .map(|p| p.join("gavno/classic-windows.json"))
}

fn remember(entries: &mut Vec<Entry>, entry: Entry) -> bool {
    if entries.last() == Some(&entry) {
        return false;
    }
    entries.retain(|e| e.app_id != entry.app_id);
    entries.push(entry);
    if entries.len() > LIMIT {
        entries.drain(..entries.len() - LIMIT);
    }
    true
}

fn load(path: &Path) -> Vec<Entry> {
    let result = (|| {
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(io::Error::other("window store is too large"));
        }
        let entries: Vec<Entry> = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if entries
            .iter()
            .any(|e| e.app_id.is_empty() || e.geometry.is_some_and(|r| r[2] <= 0 || r[3] <= 0))
        {
            return Err(io::Error::other("invalid window state"));
        }
        let mut bounded = Vec::new();
        for e in entries {
            remember(&mut bounded, e);
        }
        Ok(bounded)
    })();
    result.unwrap_or_else(|err| {
        tracing::info!(%err, "Classic window store unavailable; starting empty");
        Vec::new()
    })
}

fn save(path: &Path, entries: &[Entry]) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing state directory"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, entries).map_err(io::Error::other)?;
        file.flush()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn clamp(relative: [i32; 4], area: Rect) -> Rect {
    let w = relative[2].clamp(1, area.size.w.max(1));
    let h = relative[3].clamp(1, area.size.h.max(1));
    Rect::new(
        (
            area.loc
                .x
                .saturating_add(relative[0].clamp(0, (area.size.w - w).max(0))),
            area.loc
                .y
                .saturating_add(relative[1].clamp(0, (area.size.h - h).max(0))),
        )
            .into(),
        (w, h).into(),
    )
}

/// Longest a new window waits for the stored Classic geometry.
const STORE_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

/// Sends the held-back initial configures.
fn flush_waiting_windows(gv: &mut Gavno) {
    for w in std::mem::take(&mut gv.classic.windows.waiting) {
        if let Some(t) = w
            .toplevel()
            .filter(|t| w.alive() && !t.is_initial_configure_sent())
        {
            crate::handlers::xdg_shell::prepare_initial_configure(gv, &w);
            t.send_configure();
        }
    }
}

impl Gavno {
    #[cfg(not(test))]
    pub(crate) fn start_classic_window_store(&mut self) {
        let Some(path) = state_path(
            std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        ) else {
            return;
        };
        self.start_classic_window_store_at(path);
    }

    fn start_classic_window_store_at(&mut self, path: PathBuf) {
        use smithay::reexports::calloop::channel;
        let (loaded_tx, loaded_rx) = channel::channel();
        let (write_tx, write_rx) = mpsc::sync_channel(1);
        let pending: Arc<Mutex<Option<Vec<Entry>>>> = Arc::new(Mutex::new(None));
        let worker_pending = pending.clone();
        if let Err(err) = self.loop_handle.insert_source(loaded_rx, |event, _, gv| {
            if let channel::Event::Msg(mut entries) = event {
                // Closes that raced startup take precedence over disk state.
                let pending = std::mem::take(&mut gv.classic.windows.entries);
                let changed = !pending.is_empty();
                for e in pending {
                    remember(&mut entries, e);
                }
                gv.classic.windows.entries = entries;
                gv.classic.windows.loaded = true;
                if changed {
                    gv.persist_classic_windows();
                }
                flush_waiting_windows(gv);
            }
        }) {
            tracing::warn!(%err, "Failed to watch Classic window store");
            flush_waiting_windows(self);
            return;
        }
        match std::thread::Builder::new()
            .name("classic-windows".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let _ = loaded_tx.send(load(&path));
                drop(loaded_tx);
                while write_rx.recv().is_ok() {
                    let entries = worker_pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some(entries) = entries {
                        if let Err(err) = save(&path, &entries) {
                            tracing::warn!(%err, "Failed to save Classic window states");
                        }
                    }
                }
            }) {
            Ok(_) => {
                self.classic.windows.writer = Some(Writer {
                    pending,
                    wake: write_tx,
                })
            }
            Err(err) => {
                tracing::warn!(%err, "Failed to start Classic window store");
                flush_waiting_windows(self);
            }
        }
    }

    fn persist_classic_windows(&mut self) {
        if self.classic.windows.loaded {
            if let Some(tx) = &self.classic.windows.writer {
                if tx.submit(self.classic.windows.entries.clone()) {
                    self.classic.windows.dirty = false;
                }
            }
        }
    }

    pub(crate) fn delay_classic_configure(&mut self, w: &Window) -> bool {
        if !self.classic_windows_active()
            || self.classic.windows.loaded
            || self.classic.windows.gave_up
            || self.classic.windows.writer.is_none()
        {
            return false;
        }
        if !self.classic.windows.waiting.contains(w) {
            let first = self.classic.windows.waiting.is_empty();
            self.classic.windows.waiting.push(w.clone());
            if first {
                // One-shot, armed only while a window waits: a stuck disk must
                // not keep a client without its first configure.
                use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
                let armed = self.loop_handle.insert_source(
                    Timer::from_duration(STORE_WAIT),
                    |_, _, gv| {
                        if !gv.classic.windows.loaded {
                            gv.classic.windows.gave_up = true;
                            tracing::warn!("Classic window store is slow; configuring windows without it");
                        }
                        flush_waiting_windows(gv);
                        TimeoutAction::Drop
                    },
                );
                if armed.is_err() {
                    self.classic.windows.waiting.retain(|pending| pending != w);
                    return false;
                }
            }
        }
        true
    }

    pub(crate) fn classic_windows_active(&self) -> bool {
        self.features.on(crate::module::Feature::Classic)
            && self.classic.windows.active
            && self.greeter.is_none()
    }

    pub(crate) fn enforce_classic_floating(&mut self) {
        if !self.classic_windows_active() {
            return;
        }
        self.classic.windows.layouts.retain(|saved| {
            self.workspaces.outputs.iter().any(|o| {
                o.output == saved.output && o.list.items.iter().any(|w| w.id == saved.workspace)
            })
        });
        for owner in &mut self.workspaces.outputs {
            for ws in &mut owner.list.items {
                if !self
                    .classic
                    .windows
                    .layouts
                    .iter()
                    .any(|s| s.output == owner.output && s.workspace == ws.id)
                {
                    self.classic.windows.layouts.push(Layout {
                        output: owner.output.clone(),
                        workspace: ws.id,
                        tiling: ws.tiling,
                        tree: ws.tree.clone(),
                    });
                }
                ws.tiling = false;
            }
        }
    }

    pub(crate) fn switch_classic_windows(&mut self) {
        let enable = self.classic.settled == ShellMode::Classic;
        if self.classic.windows.active == enable {
            return;
        }
        if enable {
            // Code-mode tiles are still tiles here: leave code mode only after
            // the layouts are saved, and remember to resume it on return.
            let tiles: Vec<_> = self
                .workspaces
                .all()
                .filter(|w| self.tiled(&w.window))
                .map(|w| w.window.clone())
                .collect();
            self.classic.windows.active = true;
            self.enforce_classic_floating();
            if self.code_mode {
                self.code_mode = false;
                self.dock.code_mode = false;
                self.classic.windows.code_resume = true;
                self.classic.windows.code_saved = std::mem::take(&mut self.code_mode_saved);
                self.show_pointer();
                self.sync_bar_hints();
            }
            for w in &tiles {
                // Former tiles float in their tiling arrangement; the refit
                // gives each one its slot of the saved layout.
                with_mode_state(w, |s| s.natural_float = None);
            }
            self.classic.windows.tiles = tiles;
        } else {
            self.classic.windows.active = false;
            let saved = std::mem::take(&mut self.classic.windows.layouts);
            for owner in &mut self.workspaces.outputs {
                for ws in &mut owner.list.items {
                    if let Some(layout) = saved
                        .iter()
                        .find(|s| s.output == owner.output && s.workspace == ws.id)
                    {
                        ws.tiling = layout.tiling;
                        ws.tree = layout.tree.clone();
                    } else {
                        ws.tiling = owner.list.default_tiling;
                    }
                }
            }
            let resume = std::mem::take(&mut self.classic.windows.code_resume);
            let code_saved = std::mem::take(&mut self.classic.windows.code_saved);
            if resume && !code_saved.is_empty() {
                self.code_mode = true;
                self.dock.code_mode = true;
                self.code_mode_saved = code_saved;
                self.sync_bar_hints();
            } else if resume {
                // Code mode was switched on while Classic was active: nothing
                // was saved, so enter it the regular way.
                self.set_code_mode(true);
            } else {
                // Code mode ended during Classic: the saved layouts above still
                // carry its forced tiling, so give the workspaces their own back.
                for owner in &mut self.workspaces.outputs {
                    for ws in &mut owner.list.items {
                        if let Some((_, _, tiling)) = code_saved
                            .iter()
                            .find(|(o, id, _)| *o == owner.output && *id == ws.id)
                        {
                            ws.tiling = *tiling;
                        }
                    }
                }
            }
            let mut leaving = std::mem::take(&mut self.classic.windows.tiles);
            // Windows that got their Classic maximize in the initial configure
            // but have not mapped yet never reached `tiles`.
            for w in self
                .workspaces
                .all()
                .map(|v| &v.window)
                .chain(self.unmapped.iter())
            {
                if mode_state(w).classic_maximized && !leaving.contains(w) {
                    leaving.push(w.clone());
                }
            }
            for w in leaving {
                if !w.alive() {
                    continue;
                }
                if self.workspaces.membership(&w).is_none() {
                    // Not mapped yet: let it map as a normal window of the shell it joins.
                    if self.unmapped.contains(&w) && mode_state(&w).classic_maximized {
                        with_mode_state(&w, |s| {
                            s.mode = WindowMode::Floating;
                            s.restore = None;
                            s.classic_maximized = false;
                        });
                        configure_mode(&w, WindowMode::Floating, None);
                        if let Some(t) = w.toplevel().filter(|t| t.is_initial_configure_sent()) {
                            t.send_pending_configure();
                        }
                    }
                    continue;
                }
                // Keep fullscreen and minimized flags; minimized tiles rejoin on restore.
                let s = mode_state(&w);
                if s.mode == WindowMode::Maximized
                    && (self.tiling_workspace(&w) || !s.classic_maximized)
                {
                    with_mode_state(&w, |s| {
                        s.mode = WindowMode::Floating;
                        s.restore = None;
                    });
                    configure_mode(&w, WindowMode::Floating, None);
                }
                with_mode_state(&w, |s| s.classic_maximized = false);
            }
        }
    }

    /// One configure per non-floating window (tiles, maximized, snapped and
    /// fullscreen alike: the work area and gap differ between the shells),
    /// preserving Space order and focus.
    pub(crate) fn refit_classic_transition(&mut self) {
        let order: Vec<_> = self.space.elements().cloned().collect();
        let windows: Vec<_> = self.workspaces.all().map(|v| v.window.clone()).collect();
        // Former tiles keep their tiling arrangement over this shell's work
        // area. Only the windows converted on entry get a slot; the saved
        // layouts hold the exact tree.
        let mut slots: Vec<(Window, Rect)> = Vec::new();
        if self.classic_windows_active() {
            for layout in &self.classic.windows.layouts {
                let area = self.work_area(&layout.output);
                slots.extend(
                    layout
                        .tree
                        .layout(area, self.config.gap)
                        .0
                        .into_iter()
                        .filter(|(w, _)| self.classic.windows.tiles.contains(w)),
                );
            }
        }
        for w in windows {
            let s = mode_state(&w);
            if s.minimized {
                continue;
            }
            if let Some((_, slot)) = slots.iter().find(|(t, _)| t == &w).filter(|_| s.mode == WindowMode::Floating) {
                let rect = self.classic_client_rect(&w, *slot);
                configure_mode(&w, WindowMode::Floating, Some(rect.size));
                self.save_workspace_location(&w, rect.loc);
                if self.space.element_location(&w).is_some() {
                    self.window_animations.cancel(&w);
                    self.space.map_element(w.clone(), rect.loc, false);
                }
                if let Some(t) = w.toplevel() {
                    t.send_pending_configure();
                }
                continue;
            }
            if s.mode == WindowMode::Floating {
                continue;
            }
            let Some(output) = self.workspaces.membership(&w).map(|(o, _)| o) else {
                continue;
            };
            let area = self.work_area(&output);
            let gap = if self.classic_windows_active() {
                0
            } else {
                self.config.gap
            };
            let rect = match s.mode {
                WindowMode::Maximized => crate::snap::Zone::Maximized.rect(area, gap),
                WindowMode::Snapped(zone) => zone.rect(area, self.config.gap),
                WindowMode::Fullscreen => self.space.output_geometry(&output).unwrap_or(area),
                WindowMode::Floating => unreachable!(),
            };
            let rect = if s.mode == WindowMode::Fullscreen {
                rect
            } else {
                self.classic_client_rect(&w, rect)
            };
            configure_mode(&w, s.mode, Some(rect.size));
            self.save_workspace_location(&w, rect.loc);
            if self.space.element_location(&w).is_some() {
                self.window_animations.cancel(&w);
                self.space.map_element(w.clone(), rect.loc, false);
            }
            if let Some(t) = w.toplevel() {
                t.send_pending_configure();
            }
        }
        self.reflow_tiling(false);
        // map_element raises even with activate=false. Put every surviving
        // mapped window back in its original stacking slot after the batch.
        for w in order {
            self.space.raise_element(&w, false);
        }
    }

    fn classic_entry(&self, w: &Window) -> Option<&Entry> {
        if !self.classic_windows_active() || !normal_window(w) {
            return None;
        }
        let id = app_id(w)?;
        self.classic.windows.entries.iter().find(|e| e.app_id == id)
    }

    pub(crate) fn classic_initial_output(&self, w: &Window) -> Option<Output> {
        if mode_state(w).mode == WindowMode::Fullscreen {
            return None;
        }
        let e = self.classic_entry(w)?;
        self.space.outputs().find(|o| o.name() == e.output).cloned()
    }

    pub(crate) fn classic_initial_rect(&self, w: &Window) -> Option<Rect> {
        if mode_state(w).mode != WindowMode::Floating {
            return None;
        }
        let e = self.classic_entry(w)?;
        let mut relative = e.geometry?;
        let o = self
            .classic_initial_output(w)
            .or_else(|| self.active_output())?;
        let area = self.work_area(&o);
        let client_area = self.classic_client_rect(w, area);
        relative[1] = relative[1].saturating_sub(client_area.loc.y - area.loc.y);
        Some(clamp(relative, client_area))
    }

    pub(crate) fn prepare_classic_window(&self, w: &Window) {
        if !self.classic_windows_active() || !normal_window(w) {
            return;
        }
        let s = mode_state(w);
        // Fullscreen remains explicit. Remembered state wins over startup
        // maximize requests (clients can remember their own previous maximize).
        if s.mode == WindowMode::Fullscreen {
            return;
        }
        let entry = self.classic_entry(w);
        if s.mode != WindowMode::Floating && entry.is_none() {
            return;
        }
        if entry.is_some_and(|e| e.geometry.is_some()) {
            with_mode_state(w, |s| {
                s.mode = WindowMode::Floating;
                s.restore = None;
                s.classic_maximized = false;
            });
        }
        if let Some(rect) = self.classic_initial_rect(w) {
            configure_mode(w, WindowMode::Floating, Some(rect.size));
        } else if self.classic_entry(w).is_some() || self.config.classic.open_maximized {
            with_mode_state(w, |s| {
                s.mode = WindowMode::Maximized;
                s.classic_maximized = true;
            });
            let size = self
                .classic_initial_output(w)
                .or_else(|| self.active_output())
                .map(|o| self.classic_client_rect(w, self.work_area(&o)).size);
            configure_mode(w, WindowMode::Maximized, size);
        }
    }

    pub(crate) fn track_classic_map(&mut self, w: &Window) {
        if self.classic_windows_active() && normal_window(w) {
            self.classic.windows.tiles.retain(|w| w.alive());
            if mode_state(w).classic_maximized {
                self.classic.windows.tiles.push(w.clone());
            }
            if let Some(entry) = self.classic_entry(w).cloned() {
                self.classic.windows.dirty |= remember(&mut self.classic.windows.entries, entry);
            }
        }
    }

    pub(crate) fn forget_classic_window(&mut self, w: &Window) {
        self.classic.windows.tiles.retain(|tile| tile != w);
        self.classic.windows.waiting.retain(|pending| pending != w);
        for layout in &mut self.classic.windows.layouts {
            layout.tree.remove(w);
        }
    }

    pub(crate) fn remember_classic_window(&mut self, w: &Window) {
        if !self.classic_windows_active() || !normal_window(w) {
            return;
        }
        let Some(id) = app_id(w) else { return };
        let Some((output, _)) = self.workspaces.membership(w) else {
            return;
        };
        let s = mode_state(w);
        let geometry = match s.mode {
            WindowMode::Maximized => None,
            WindowMode::Floating | WindowMode::Snapped(_) => {
                let area = self.work_area(&output);
                let loc = self
                    .space
                    .element_location(w)
                    .or_else(|| self.workspaces.location(w))
                    .unwrap_or(area.loc);
                let size = w
                    .toplevel()
                    .and_then(|t| t.with_pending_state(|s| s.size))
                    .unwrap_or(w.geometry().size);
                Some([
                    loc.x.saturating_sub(area.loc.x),
                    loc.y.saturating_sub(area.loc.y),
                    size.w.max(1),
                    size.h.max(1),
                ])
            }
            // Fullscreen is deliberately not remembered as an application default.
            WindowMode::Fullscreen => return,
        };
        self.classic.windows.dirty |= remember(
            &mut self.classic.windows.entries,
            Entry {
                app_id: id,
                output: output.name(),
                geometry,
            },
        );
        if self.classic.windows.dirty {
            self.persist_classic_windows();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{snap::Direction, state::tests::Harness};
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;

    fn output(h: &mut Harness, name: &str, x: i32) -> Output {
        let output = Output::new(
            name.into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: (1600, 1000).into(),
                refresh: 120000,
            }),
            None,
            None,
            None,
        );
        h.gv.space.map_output(&output, (x, 0));
        h.gv.workspaces.ensure(&output);
        output
    }
    fn settle(h: &mut Harness, mode: ShellMode) {
        h.gv.set_shell_mode(mode);
        h.gv.classic.progress.jump(mode.progress());
        h.gv.settle_classic();
    }
    fn mapped(h: &mut Harness) -> Window {
        let w = h.window();
        crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &w);
        w.toplevel().unwrap().send_configure();
        h.gv.map_new_window(w.clone());
        w
    }
    fn id(w: &Window, value: &str) {
        with_states(w.toplevel().unwrap().wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .app_id = Some(value.into());
        });
    }
    fn geometry(w: &Window) -> Option<smithay::utils::Size<i32, smithay::utils::Logical>> {
        w.toplevel().unwrap().with_pending_state(|s| s.size)
    }
    fn configure_count(w: &Window) -> usize {
        with_states(w.toplevel().unwrap().wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .pending_configures()
                .len()
        })
    }
    fn tree(h: &Harness, output: &Output) -> Tree<Window> {
        h.gv.workspaces.get(output).unwrap().list.items[0]
            .tree
            .clone()
    }

    #[test]
    fn tiles_round_trip_exact_tree_ratios_stacking_focus_and_no_extra_configures() {
        let mut h = Harness::new();
        let o = output(&mut h, "first", 0);
        h.gv.workspaces.ensure(&o).list.items[0].tiling = true;
        let a = mapped(&mut h);
        let b = mapped(&mut h);
        let c = mapped(&mut h);
        h.gv.resize_tile(&a, Direction::Right, 137, false);
        h.gv.rotate_tile();
        h.gv.focus_window(Some(&b));
        let before = tree(&h, &o);
        let layout = h.gv.workspaces.get(&o).unwrap().list.items[0]
            .tree
            .layout(h.gv.work_area(&o), h.gv.config.gap)
            .0;
        let order: Vec<_> = h.gv.space.elements().cloned().collect();
        let counts: Vec<_> = [&a, &b, &c].into_iter().map(configure_count).collect();
        settle(&mut h, ShellMode::Classic);
        for (w, count) in [&a, &b, &c].into_iter().zip(counts) {
            assert_eq!(configure_count(w), count + 1);
        }
        assert!(h
            .gv
            .workspaces
            .outputs
            .iter()
            .all(|o| o.list.items.iter().all(|w| !w.tiling)));
        assert_tile_slots(&h, &o, &before, &[a.clone(), b.clone(), c.clone()]);
        assert_eq!(h.gv.space.elements().cloned().collect::<Vec<_>>(), order);
        assert_eq!(h.gv.focused_window(), Some(b.clone()));
        // Unrelated reflows in Classic must not consume floating restores.
        let redraws = h.gv.redraw_requests;
        h.gv.reflow_tiling(true);
        assert_eq!(h.gv.redraw_requests, redraws);
        let counts: Vec<_> = [&a, &b, &c].into_iter().map(configure_count).collect();
        settle(&mut h, ShellMode::Gavno);
        for (w, count) in [&a, &b, &c].into_iter().zip(counts) {
            assert_eq!(configure_count(w), count + 1);
        }
        assert_eq!(tree(&h, &o), before);
        assert_eq!(
            h.gv.workspaces.get(&o).unwrap().list.items[0]
                .tree
                .layout(h.gv.work_area(&o), h.gv.config.gap)
                .0,
            layout
        );
        assert_eq!(h.gv.space.elements().cloned().collect::<Vec<_>>(), order);
        assert_eq!(h.gv.focused_window(), Some(b));
        assert!(!h.gv.code_mode);
        for w in [&a, &c] {
            assert!(h.gv.tiled(w));
        }
    }

    /// Every former tile floats in the tiling arrangement over the Classic work area.
    fn assert_tile_slots(h: &Harness, o: &Output, before: &Tree<Window>, tiles: &[Window]) {
        let layout = before.layout(h.gv.work_area(o), h.gv.config.gap).0;
        assert_eq!(layout.len(), tiles.len());
        for (w, rect) in layout {
            assert!(tiles.contains(&w));
            let slot = h.gv.classic_client_rect(&w, rect);
            assert_eq!(mode_state(&w).mode, WindowMode::Floating);
            assert_eq!(geometry(&w), Some(slot.size));
            assert_eq!(h.gv.space.element_location(&w), Some(slot.loc));
            assert!(w.toplevel().unwrap().with_pending_state(|s| {
                !s.states.contains(State::Maximized) && !s.states.contains(State::TiledLeft)
            }));
        }
    }

    fn same_app_tiles_keep_arrangement_and_return_to_tiles(count: usize, remembered: bool) {
        let mut h = Harness::new();
        let o = output(&mut h, "first", 0);
        h.gv.workspaces.ensure(&o).list.items[0].tiling = true;
        let mut tiles = Vec::new();
        for _ in 0..count {
            // Map through the real protocol: initial commit, configure, buffer.
            let w = h.window();
            id(&w, "foot");
            // foot negotiates xdg-decoration before its first commit and asks
            // for server-side decorations, which the handler answers client-side.
            {
                use smithay::{
                    reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
                    wayland::shell::xdg::decoration::XdgDecorationHandler,
                };
                h.gv.new_decoration(w.toplevel().unwrap().clone());
                h.gv.request_mode(w.toplevel().unwrap().clone(), Mode::ServerSide);
            }
            h.gv.unmapped.push(w.clone());
            h.commit_state(&w, None);
            let size = geometry(&w).unwrap_or((800, 600).into());
            h.commit_size(&w, size.w, size.h);
            assert!(h.gv.space.element_location(&w).is_some());
            tiles.push(w);
        }
        if remembered {
            h.gv.classic.windows.entries.push(Entry {
                app_id: "foot".into(),
                output: "first".into(),
                geometry: None,
            });
        }
        for w in &tiles {
            let size = geometry(w).unwrap();
            h.commit_size(w, size.w, size.h);
        }
        let before = tree(&h, &o);
        let order: Vec<_> = h.gv.space.elements().cloned().collect();
        // Like the real loop: the spring runs, idle callbacks tick, then it settles.
        h.gv.set_shell_mode(ShellMode::Classic);
        for _ in 0..400 {
            h.gv.on_loop_idle();
            if h.gv.classic.settled == ShellMode::Classic {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(h.gv.classic.settled, ShellMode::Classic);
        assert_tile_slots(&h, &o, &before, &tiles);
        // The client re-requests server-side mode; the handler answers client-side
        // and the next idle pass must refit it again.
        for w in &tiles {
            use smithay::{
                reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
                wayland::shell::xdg::decoration::XdgDecorationHandler,
            };
            h.gv.request_mode(w.toplevel().unwrap().clone(), Mode::ServerSide);
        }
        // The clients ack and commit the configures, then the loop goes idle.
        for w in &tiles {
            let size = geometry(w).unwrap();
            h.commit_size(w, size.w, size.h);
        }
        h.gv.on_loop_idle();
        assert_tile_slots(&h, &o, &before, &tiles);
        for w in &tiles {
            // The state the client actually acknowledged must be floating too.
            let acked = with_states(w.toplevel().unwrap().wl_surface(), |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .current_server_state()
                    .clone()
            });
            assert!(!acked.states.contains(State::Maximized));
            assert_eq!(acked.size, geometry(w));
        }
        assert_eq!(h.gv.space.elements().cloned().collect::<Vec<_>>(), order);
        settle(&mut h, ShellMode::Gavno);
        assert_eq!(tree(&h, &o), before);
        for w in &tiles {
            assert!(h.gv.tiled(w));
        }
    }

    #[test]
    fn window_launched_in_classic_joins_tiling_when_it_maps_after_returning() {
        let mut h = Harness::new();
        let o = output(&mut h, "first", 0);
        h.gv.workspaces.ensure(&o).list.items[0].tiling = true;
        settle(&mut h, ShellMode::Classic);
        let w = h.window();
        crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &w);
        w.toplevel().unwrap().send_configure();
        h.gv.unmapped.push(w.clone());
        assert_eq!(mode_state(&w).mode, WindowMode::Maximized);
        settle(&mut h, ShellMode::Gavno);
        h.gv.unmapped.retain(|u| u != &w);
        h.gv.map_new_window(w.clone());
        assert!(!mode_state(&w).classic_maximized);
        assert!(h.gv.tiled(&w), "{:?}", mode_state(&w));
        assert!(!w
            .toplevel()
            .unwrap()
            .with_pending_state(|s| s.states.contains(State::Maximized)));
    }

    #[test]
    fn maximized_windows_are_refitted_to_the_work_area_of_each_shell() {
        let mut h = Harness::new();
        let o = output(&mut h, "first", 0);
        assert!(h.gv.config.gap > 0);
        let gavno = mapped(&mut h);
        h.gv.set_window_mode(&gavno, WindowMode::Maximized, None);
        let gap = h.gv.config.gap;
        let size = |h: &Harness, gap| crate::snap::Zone::Maximized.rect(h.gv.work_area(&o), gap).size;
        assert_eq!(geometry(&gavno), Some(size(&h, gap)));
        settle(&mut h, ShellMode::Classic);
        let classic = mapped(&mut h);
        assert_eq!(geometry(&gavno), Some(size(&h, 0)));
        assert_eq!(geometry(&classic), Some(size(&h, 0)));
        settle(&mut h, ShellMode::Gavno);
        assert_eq!(geometry(&gavno), Some(size(&h, gap)));
        assert_eq!(geometry(&classic), Some(size(&h, gap)));
    }

    fn code_mode_tiles_float_in_classic_and_return(count: usize, code_on: bool) {
        let mut h = Harness::new();
        let o = output(&mut h, "first", 0);
        // The workspace itself floats; code mode forces the tiling.
        assert!(!h.gv.workspaces.ensure(&o).list.items[0].tiling);
        if code_on {
            h.gv.set_code_mode(true);
        } else {
            h.gv.workspaces.ensure(&o).list.items[0].tiling = true;
        }
        let tiles: Vec<_> = (0..count).map(|_| mapped(&mut h)).collect();
        for w in &tiles {
            assert!(h.gv.tiled(w));
        }
        let before = tree(&h, &o);
        let order: Vec<_> = h.gv.space.elements().cloned().collect();
        h.gv.set_shell_mode(ShellMode::Classic);
        // Entering Classic must not release the tiles before the transition.
        assert!(tiles.iter().all(|w| h.gv.tiled(w)));
        h.gv.classic.progress.jump(1.0);
        h.gv.settle_classic();
        assert!(!h.gv.code_mode);
        assert_tile_slots(&h, &o, &before, &tiles);
        assert_eq!(h.gv.space.elements().cloned().collect::<Vec<_>>(), order);
        settle(&mut h, ShellMode::Gavno);
        assert_eq!(h.gv.code_mode, code_on);
        assert_eq!(tree(&h, &o), before);
        for w in &tiles {
            assert!(h.gv.tiled(w));
        }
        if code_on {
            // Code mode keeps its own saved state: leaving it floats the workspace again.
            h.gv.set_code_mode(false);
            assert!(!h.gv.workspaces.get(&o).unwrap().list.items[0].tiling);
        }
    }

    #[test]
    fn code_mode_tiles_keep_their_arrangement_in_classic_and_code_mode_resumes() {
        for count in [2, 3] {
            code_mode_tiles_float_in_classic_and_return(count, true);
            code_mode_tiles_float_in_classic_and_return(count, false);
        }
    }

    #[test]
    fn code_mode_requests_made_in_classic_apply_on_return() {
        let mut h = Harness::new();
        let o = output(&mut h, "first", 0);
        // Ends in Classic: code mode off there keeps workspaces floating afterwards.
        h.gv.workspaces.ensure(&o).list.items[0].tiling = false;
        h.gv.set_code_mode(true);
        settle(&mut h, ShellMode::Classic);
        assert!(h.gv.classic.windows.code_resume);
        h.gv.set_code_mode(false);
        assert!(!h.gv.classic.windows.code_resume);
        settle(&mut h, ShellMode::Gavno);
        assert!(!h.gv.code_mode);
        assert!(!h.gv.workspaces.ensure(&o).list.items[0].tiling, "saved floating state comes back");
        // Enabled while in Classic: resumes on return, and disabling restores the old state.
        settle(&mut h, ShellMode::Classic);
        h.gv.set_code_mode(true);
        assert!(!h.gv.code_mode, "Classic itself stays out of code mode");
        settle(&mut h, ShellMode::Gavno);
        assert!(h.gv.code_mode);
        assert!(h.gv.workspaces.ensure(&o).list.items.iter().all(|ws| ws.tiling));
        h.gv.set_code_mode(false);
        assert!(!h.gv.workspaces.ensure(&o).list.items[0].tiling);
    }

    #[test]
    fn windows_do_not_wait_forever_for_a_stuck_store() {
        let mut h = Harness::new();
        output(&mut h, "first", 0);
        settle(&mut h, ShellMode::Classic);
        let w = h.window();
        h.gv.unmapped.push(w.clone());
        // A writer without a loader: the load never answers.
        let (wake, _keep) = mpsc::sync_channel(1);
        h.gv.classic.windows.writer = Some(Writer { pending: Arc::default(), wake });
        crate::handlers::xdg_shell::handle_commit(&mut h.gv, w.toplevel().unwrap().wl_surface());
        assert!(!w.toplevel().unwrap().is_initial_configure_sent());
        for _ in 0..200 {
            h.event_loop
                .dispatch(std::time::Duration::from_millis(5), &mut h.gv)
                .unwrap();
            if w.toplevel().unwrap().is_initial_configure_sent() {
                break;
            }
        }
        assert!(w.toplevel().unwrap().is_initial_configure_sent());
        assert!(!h.gv.classic.windows.loaded, "persisting stays gated on the real load");
    }

    #[test]
    fn snapped_windows_stay_snapped_in_classic_and_return_to_their_snaps() {
        use crate::snap::Zone;
        let mut h = Harness::new();
        let o = output(&mut h, "first", 0);
        let zones = [Zone::Left, Zone::Right, Zone::TopLeft, Zone::BottomRight];
        let windows: Vec<_> = zones
            .iter()
            .map(|z| {
                let w = mapped(&mut h);
                h.gv.set_window_mode(&w, WindowMode::Snapped(*z), None);
                w
            })
            .collect();
        let rects: Vec<_> = windows.iter().map(geometry).collect();
        settle(&mut h, ShellMode::Classic);
        for (w, zone) in windows.iter().zip(zones) {
            let slot = h.gv.classic_client_rect(w, zone.rect(h.gv.work_area(&o), h.gv.config.gap));
            assert_eq!(mode_state(w).mode, WindowMode::Snapped(zone));
            assert_eq!(geometry(w), Some(slot.size));
            assert_eq!(h.gv.space.element_location(w), Some(slot.loc));
        }
        settle(&mut h, ShellMode::Gavno);
        for ((w, zone), size) in windows.iter().zip(zones).zip(rects) {
            assert_eq!(mode_state(w).mode, WindowMode::Snapped(zone));
            assert_eq!(geometry(w), size);
        }
    }

    #[test]
    fn same_app_tiles_keep_arrangement_with_remembered_state() {
        same_app_tiles_keep_arrangement_and_return_to_tiles(2, true);
        same_app_tiles_keep_arrangement_and_return_to_tiles(3, true);
    }

    #[test]
    fn same_app_tiles_keep_arrangement_without_remembered_state() {
        same_app_tiles_keep_arrangement_and_return_to_tiles(2, false);
        same_app_tiles_keep_arrangement_and_return_to_tiles(3, false);
    }

    #[test]
    fn preexisting_overrides_fixed_and_minimized_windows_keep_state_and_geometry() {
        let mut h = Harness::new();
        let o = output(&mut h, "first", 0);
        let floating = mapped(&mut h);
        h.commit_size(&floating, 500, 300);
        h.gv.space.map_element(floating.clone(), (130, 170), false);
        h.gv.save_workspace_location(&floating, (130, 170).into());
        with_mode_state(&floating, |s| s.tile_floating = true);
        let maximized = mapped(&mut h);
        h.gv.set_window_mode(&maximized, WindowMode::Maximized, None);
        let full = mapped(&mut h);
        h.gv.set_window_mode(&full, WindowMode::Fullscreen, None);
        let minimized = mapped(&mut h);
        h.commit_size(&minimized, 400, 200);
        h.gv.minimize_window(&minimized);
        h.gv.workspaces.ensure(&o).list.items[0].tiling = true;
        h.gv.reflow_tiling(false);
        let tile = mapped(&mut h);
        let preserved: Vec<_> = [&floating, &maximized, &full, &minimized]
            .into_iter()
            .map(|w| {
                (
                    w.clone(),
                    mode_state(w),
                    h.gv.workspaces.location(w),
                    geometry(w),
                )
            })
            .collect();
        let order: Vec<_> = h.gv.space.elements().cloned().collect();
        settle(&mut h, ShellMode::Classic);
        assert_eq!(mode_state(&tile).mode, WindowMode::Floating);
        for (w, state, location, size) in &preserved {
            assert_eq!(mode_state(w), *state);
            // Maximized windows follow the shell's work area and gap.
            if w != &maximized {
                assert_eq!(h.gv.workspaces.location(w), *location);
                assert_eq!(geometry(w), *size);
            }
        }
        let slot = crate::snap::Zone::Maximized.rect(h.gv.work_area(&o), 0);
        assert_eq!(geometry(&maximized), Some(slot.size));
        assert_eq!(h.gv.space.elements().cloned().collect::<Vec<_>>(), order);
        settle(&mut h, ShellMode::Gavno);
        for (w, state, location, size) in &preserved {
            assert_eq!(mode_state(w), *state);
            assert_eq!(h.gv.workspaces.location(w), *location);
            assert_eq!(geometry(w), *size);
        }
        assert!(h.gv.tiled(&tile));
    }

    #[test]
    fn new_classic_windows_maximize_except_parented_and_fixed_and_reload_applies() {
        let mut h = Harness::new();
        let o = output(&mut h, "first", 0);
        settle(&mut h, ShellMode::Classic);
        let a = mapped(&mut h);
        assert_eq!(mode_state(&a).mode, WindowMode::Maximized);
        assert_eq!(geometry(&a), Some(h.gv.work_area(&o).size));
        let dialog = h.window();
        with_states(dialog.toplevel().unwrap().wl_surface(), |s| {
            s.data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .parent = Some(a.toplevel().unwrap().wl_surface().clone());
        });
        crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &dialog);
        assert_eq!(mode_state(&dialog).mode, WindowMode::Floating);
        assert!(geometry(&dialog).is_none());
        let fixed = h.window();
        with_states(fixed.toplevel().unwrap().wl_surface(), |s| {
            let mut cached = s.cached_state.get::<SurfaceCachedState>();
            let c = cached.current();
            c.min_size = (400, 200).into();
            c.max_size = c.min_size;
        });
        crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &fixed);
        assert_eq!(mode_state(&fixed).mode, WindowMode::Floating);
        assert!(geometry(&fixed).is_none());
        let next = crate::config::Config::build(
            true,
            Some("[classic]\nopen_maximized = false\n[tiling]\ndefault = true"),
            |_| None,
        )
        .0;
        let prepared = crate::config_reload::Prepared::new(next, &h.gv.config).unwrap();
        crate::config_reload::apply(&mut h.gv, prepared).unwrap();
        assert!(!h.gv.config.classic.open_maximized);
        let natural = mapped(&mut h);
        assert_eq!(mode_state(&natural).mode, WindowMode::Floating);
        assert!(geometry(&natural).is_none());
        // No tiling toggle can leak a tiled workspace into Classic.
        h.gv.toggle_workspace_tiling(&o);
        assert!(!h.gv.workspaces.get(&o).unwrap().list.items[0].tiling);
    }

    #[test]
    fn closed_app_restores_floating_geometry_on_its_output_even_with_maximize_disabled() {
        let mut h = Harness::new();
        output(&mut h, "first", 0);
        let second = output(&mut h, "second", 1600);
        settle(&mut h, ShellMode::Classic);
        let a = mapped(&mut h);
        id(&a, "editor");
        h.gv.set_window_mode(&a, WindowMode::Floating, None);
        let area = h.gv.work_area(&second);
        let rect = Rect::new((area.loc.x + 70, area.loc.y + 90).into(), (640, 480).into());
        h.gv.space.map_element(a.clone(), rect.loc, false);
        a.toplevel()
            .unwrap()
            .with_pending_state(|s| s.size = Some(rect.size));
        h.gv.refresh_workspaces();
        h.gv.unmap_window(&a);
        assert_eq!(h.gv.classic.windows.entries.len(), 1);
        h.gv.config.classic.open_maximized = false;
        let b = h.window();
        id(&b, "editor");
        // Clients may request maximize from their own saved startup state.
        h.gv.set_window_mode(&b, WindowMode::Maximized, None);
        crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &b);
        h.gv.map_new_window(b.clone());
        assert_eq!(h.gv.workspaces.membership(&b).unwrap().0, second);
        assert_eq!(mode_state(&b).mode, WindowMode::Floating);
        assert_eq!(geometry(&b), Some(rect.size));
        assert_eq!(h.gv.space.element_location(&b), Some(rect.loc));
        // A disconnected saved output falls back to a live one and clamps size/position.
        h.gv.space.unmap_output(&second);
        h.gv.classic.windows.entries[0].geometry = Some([i32::MAX, i32::MAX, 4000, 4000]);
        let c = h.window();
        id(&c, "editor");
        crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &c);
        let output = h.gv.active_output().unwrap();
        let rect = h.gv.classic_initial_rect(&c).unwrap();
        assert_eq!(rect, h.gv.work_area(&output));
    }

    #[test]
    fn output_removal_recreation_moves_closes_new_workspaces_and_quick_toggles() {
        let mut h = Harness::new();
        let first = output(&mut h, "first", 0);
        h.gv.workspaces.default_tiling = true;
        h.gv.workspaces.ensure(&first).list.default_tiling = true;
        h.gv.workspaces.ensure(&first).list.items[0].tiling = true;
        let a = mapped(&mut h);
        let b = mapped(&mut h);
        let second = output(&mut h, "second", 1600);
        h.gv.workspaces.ensure(&second).list.items[0].tiling = false;
        let original = tree(&h, &first);
        // Reversals before settling must leave the original layout untouched.
        for _ in 0..5 {
            h.gv.set_shell_mode(ShellMode::Classic);
            h.gv.set_shell_mode(ShellMode::Gavno);
        }
        h.gv.classic.progress.jump(0.0);
        h.gv.settle_classic();
        assert_eq!(tree(&h, &first), original);
        settle(&mut h, ShellMode::Classic);
        let c = mapped(&mut h);
        assert!(h.gv.move_to_workspace(&b, 1));
        h.gv.reflow_tiling(false);
        assert!(h
            .gv
            .workspaces
            .outputs
            .iter()
            .all(|o| o.list.items.iter().all(|w| !w.tiling)));
        h.gv.unmap_window(&a);
        h.gv.space.unmap_output(&second);
        h.gv.refresh_workspaces();
        let recreated = output(&mut h, "second", 1600);
        h.gv.reflow_tiling(false);
        settle(&mut h, ShellMode::Gavno);
        assert!(h.gv.tiled(&b));
        assert!(h.gv.tiled(&c));
        assert!(h.gv.workspaces.get(&first).unwrap().list.items[0]
            .tree
            .contains(&c));
        assert!(!h.gv.workspaces.get(&first).unwrap().list.items[0]
            .tree
            .contains(&a));
        assert!(!h.gv.workspaces.get(&first).unwrap().list.items[0]
            .tree
            .contains(&b));
        assert!(h.gv.workspaces.get(&recreated).unwrap().list.items[0].tiling);
        for _ in 0..5 {
            let before = tree(&h, &first);
            settle(&mut h, ShellMode::Classic);
            settle(&mut h, ShellMode::Gavno);
            assert_eq!(tree(&h, &first), before);
        }
    }

    #[test]
    fn removed_output_with_windows_uses_surviving_identity_and_fullscreen_stays_fullscreen() {
        let mut h = Harness::new();
        let first = output(&mut h, "same-name", 0);
        h.gv.workspaces.ensure(&first).list.items[0].tiling = true;
        let tile = mapped(&mut h);
        let full = mapped(&mut h);
        h.gv.set_window_mode(&full, WindowMode::Fullscreen, None);
        let second = output(&mut h, "survivor", 1600);
        h.gv.workspaces.ensure(&second).list.items[0].tiling = true;
        settle(&mut h, ShellMode::Classic);
        h.gv.space.unmap_output(&first);
        h.gv.refresh_workspaces();
        assert_eq!(h.gv.workspaces.membership(&tile).unwrap().0, second);
        let replacement = output(&mut h, "same-name", 0);
        h.gv.reflow_tiling(false);
        settle(&mut h, ShellMode::Gavno);
        assert!(h.gv.tiled(&tile));
        assert_eq!(mode_state(&full).mode, WindowMode::Fullscreen);
        assert!(!h.gv.workspaces.get(&replacement).unwrap().list.items[0].tiling);
    }

    #[test]
    fn starting_classic_suspends_default_tiling_and_leaves_code_off() {
        let mut event_loop = smithay::reexports::calloop::EventLoop::try_new().unwrap();
        let display = smithay::reexports::wayland_server::Display::new().unwrap();
        let config = crate::config::Config::parse_checked(true, "[classic]\nstart_in_classic = true\n[tiling]\ndefault = true\n[keyboard]\ncode_mode = true", |_| None).unwrap();
        let mut gv = Gavno::new(&mut event_loop, display, config);
        let mut h = Harness::new();
        let o = output(&mut h, "first", 0);
        gv.space.map_output(&o, (0, 0));
        gv.workspaces.ensure(&o);
        gv.reflow_tiling(false);
        assert!(!gv.workspaces.get(&o).unwrap().list.items[0].tiling);
        assert!(!gv.code_mode);
        gv.set_shell_mode(ShellMode::Gavno);
        gv.classic.progress.jump(0.0);
        gv.settle_classic();
        assert!(gv.workspaces.get(&o).unwrap().list.items[0].tiling);
    }

    #[test]
    fn startup_worker_loads_before_first_configure_and_writes_only_on_close() {
        let root =
            std::env::temp_dir().join(format!("gavno-classic-worker-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = state_path(Some(root.clone()), None).unwrap();
        let entry = Entry {
            app_id: "editor".into(),
            output: "first".into(),
            geometry: Some([30, 40, 600, 400]),
        };
        save(&path, &[entry.clone()]).unwrap();
        let mut h = Harness::new();
        output(&mut h, "first", 0);
        settle(&mut h, ShellMode::Classic);
        let w = h.window();
        id(&w, "editor");
        h.gv.unmapped.push(w.clone());
        h.gv.start_classic_window_store_at(path.clone());
        crate::handlers::xdg_shell::handle_commit(&mut h.gv, w.toplevel().unwrap().wl_surface());
        assert!(!w.toplevel().unwrap().is_initial_configure_sent());
        for _ in 0..200 {
            h.event_loop
                .dispatch(std::time::Duration::from_millis(5), &mut h.gv)
                .unwrap();
            if h.gv.classic.windows.loaded {
                break;
            }
        }
        assert!(h.gv.classic.windows.loaded);
        assert!(w.toplevel().unwrap().is_initial_configure_sent());
        assert_eq!(geometry(&w), Some((600, 400).into()));
        assert_eq!(load(&path), vec![entry]);
        h.gv.unmapped.retain(|unmapped| unmapped != &w);
        h.gv.map_new_window(w.clone());
        h.gv.space.map_element(w.clone(), (80, 90), false);
        h.gv.save_workspace_location(&w, (80, 90).into());
        h.gv.unmap_window(&w);
        let expected = h.gv.classic.windows.entries.clone();
        for _ in 0..200 {
            if load(&path) == expected {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(load(&path), expected);
        drop(h);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persistence_round_trip_corruption_bounds_lru_and_no_unchanged_write() {
        let root = std::env::temp_dir().join(format!("gavno-classic-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = state_path(Some(root.clone()), None).unwrap();
        assert!(load(&path).is_empty());
        let mut entries = Vec::new();
        for i in 0..LIMIT + 1 {
            assert!(remember(
                &mut entries,
                Entry {
                    app_id: format!("app{i}"),
                    output: "first".into(),
                    geometry: Some([10, 20, 600, 400])
                }
            ));
        }
        assert_eq!(entries.len(), LIMIT);
        assert_eq!(entries[0].app_id, "app1");
        let latest = entries.last().unwrap().clone();
        assert!(!remember(&mut entries, latest));
        let first = entries[0].clone();
        assert!(remember(&mut entries, first.clone()));
        assert_eq!(entries.last(), Some(&first));
        save(&path, &entries).unwrap();
        assert_eq!(load(&path), entries);
        fs::write(&path, b"{broken").unwrap();
        assert!(load(&path).is_empty());
        fs::write(
            &path,
            br#"[{"app_id":"bad","output":"first","geometry":[0,0,-1,2]}]"#,
        )
        .unwrap();
        assert!(load(&path).is_empty());
        assert_eq!(
            state_path(None, Some(root.clone())).unwrap(),
            root.join(".local/state/gavno/classic-windows.json")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
