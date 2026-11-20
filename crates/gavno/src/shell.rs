//! One shell mode per output. The mode fields on `Gavno` (`classic.mode`,
//! `classic.settled`, the springs, `tty.on`, ...) always hold the shell of one
//! output, the *loaded* one: the output under the pointer, which is where keys,
//! gestures and new windows go. Every other output keeps its shell in
//! `Shells::stored` and is swapped in while it is drawn, settled or reflowed.
//!
//! `with_shell` also hides the other outputs' workspaces, so the mode code,
//! which walks every workspace, only touches the output it acts for. With one
//! output nothing is ever swapped or hidden.

use std::collections::HashMap;

use smithay::{desktop::Window, output::Output};

use crate::{
    classic::{windows::WindowsShell, ShellMode},
    session_mode::Mode,
    ui::spring::Spring,
    workspace::OutputWorkspaces,
    Gavno,
};

/// Everything of a shell that is per output.
pub(crate) struct ShellContext {
    mode: ShellMode,
    settled: ShellMode,
    progress: Spring,
    held: bool,
    shown: Vec<Window>,
    windows: WindowsShell,
    tty_on: bool,
    tty_saved: Vec<(Output, u64, bool)>,
    tty_progress: Spring,
    tty_held: bool,
}

impl ShellContext {
    fn mode(&self) -> Mode {
        if self.settled == ShellMode::Classic {
            Mode::Classic
        } else if self.tty_on {
            Mode::Tty
        } else {
            Mode::Gavno
        }
    }
    /// A settle pass would change something.
    fn unsettled(&self) -> bool {
        !self.held && !self.progress.is_active() && self.settled != self.mode
    }
}

#[derive(Default)]
pub(crate) struct Shells {
    /// Whose shell the `Gavno` fields hold; `None` until the first output.
    loaded: Option<String>,
    stored: HashMap<String, ShellContext>,
    /// Inside `with_shell`: other outputs' workspaces are hidden.
    scoped: bool,
    /// Modes remembered per connector from the last session.
    pub(crate) restore: HashMap<String, Mode>,
    /// What the state file holds now.
    pub(crate) persisted: Vec<(String, Mode)>,
}

impl Shells {
    #[cfg(test)]
    pub(crate) fn loaded(&self) -> Option<&str> {
        self.loaded.as_deref()
    }
}

impl Gavno {
    fn take_shell_context(&mut self) -> ShellContext {
        ShellContext {
            mode: self.classic.mode,
            settled: self.classic.settled,
            progress: self.classic.progress.clone(),
            held: self.classic.held,
            shown: std::mem::take(&mut self.classic.shown),
            windows: self.classic.windows.take_shell(),
            tty_on: self.tty.on,
            tty_saved: std::mem::take(&mut self.tty.saved),
            tty_progress: self.tty.progress.clone(),
            tty_held: self.tty.held,
        }
    }

    fn put_shell_context(&mut self, context: ShellContext) {
        self.classic.mode = context.mode;
        self.classic.settled = context.settled;
        self.classic.progress = context.progress;
        self.classic.held = context.held;
        self.classic.shown = context.shown;
        self.classic.windows.put_shell(context.windows);
        self.tty.on = context.tty_on;
        self.tty.saved = context.tty_saved;
        self.tty.progress = context.tty_progress;
        self.tty.held = context.tty_held;
    }

    /// The shell a newly seen output starts with: the loaded one as it will
    /// settle, without its layouts (they belong to other workspaces).
    fn fresh_shell_context(&self) -> ShellContext {
        let settled = self.classic.mode;
        let mut progress = self.classic.progress.clone();
        progress.jump(settled.progress());
        let mut tty_progress = self.tty.progress.clone();
        tty_progress.jump(if self.tty.on { 1.0 } else { 0.0 });
        ShellContext {
            mode: settled,
            settled,
            progress,
            held: false,
            shown: Vec::new(),
            windows: WindowsShell::settled(settled == ShellMode::Classic),
            tty_on: self.tty.on,
            tty_saved: Vec::new(),
            tty_progress,
            tty_held: false,
        }
    }

    /// Makes `name`'s shell the loaded one and returns the previous owner.
    pub(crate) fn shell_load(&mut self, name: &str) -> Option<String> {
        let previous = self.shells.loaded.clone();
        if previous.as_deref() == Some(name) {
            return previous;
        }
        let Some(previous_name) = previous.clone() else {
            // The session's shell becomes the first output's.
            self.shells.loaded = Some(name.to_owned());
            return None;
        };
        // Not seen before: start like the shell that is loaded now.
        let fresh = (!self.shells.stored.contains_key(name)).then(|| self.fresh_shell_context());
        let current = self.take_shell_context();
        self.shells.stored.insert(previous_name, current);
        let next = match fresh {
            Some(context) => context,
            None => self.shells.stored.remove(name).expect("stored shell"),
        };
        self.put_shell_context(next);
        self.shells.loaded = Some(name.to_owned());
        previous
    }

    /// Runs `f` with `output`'s shell loaded and only its workspaces visible.
    pub(crate) fn with_shell<R>(&mut self, output: &Output, f: impl FnOnce(&mut Self) -> R) -> R {
        let name = output.name();
        // Nested: the scope already narrowed everything to one output. Before
        // the pointer picked an output, the shell is still the whole session's.
        if self.shells.scoped || self.shells.loaded.is_none() {
            return f(self);
        }
        let previous = self.shell_load(&name);
        let all = std::mem::take(&mut self.workspaces.outputs);
        let mut hidden: Vec<(usize, OutputWorkspaces)> = Vec::new();
        for (index, owner) in all.into_iter().enumerate() {
            if owner.output == *output {
                self.workspaces.outputs.push(owner);
            } else {
                hidden.push((index, owner));
            }
        }
        self.shells.scoped = true;
        let result = f(self);
        self.shells.scoped = false;
        // Entries created for hidden outputs inside the scope lose to the
        // hidden originals; their windows join the original's active workspace.
        let mut visible = std::mem::take(&mut self.workspaces.outputs);
        for (_, owner) in &mut hidden {
            if let Some(i) = visible.iter().position(|v| v.output == owner.output) {
                let extra = visible.remove(i);
                let active = owner.list.active;
                for ws in extra.list.items {
                    owner.list.items[active].windows.extend(ws.windows);
                }
            }
        }
        let mut merged = Vec::with_capacity(visible.len() + hidden.len());
        let mut hidden = hidden.into_iter().peekable();
        let mut visible = visible.into_iter();
        let mut position = 0;
        loop {
            if hidden.peek().is_some_and(|(index, _)| *index == position) {
                merged.push(hidden.next().unwrap().1);
            } else if let Some(owner) = visible.next() {
                merged.push(owner);
            } else if let Some((_, owner)) = hidden.next() {
                merged.push(owner);
            } else {
                break;
            }
            position += 1;
        }
        self.workspaces.outputs = merged;
        if let Some(previous) = previous {
            self.shell_load(&previous);
        }
        result
    }

    /// The output whose shell is loaded, if it still exists.
    pub(crate) fn loaded_output(&self) -> Option<Output> {
        let name = self.shells.loaded.as_deref()?;
        self.workspaces.outputs.iter().map(|o| &o.output).find(|o| o.name() == name).cloned()
    }

    /// Loads `output`'s shell just for drawing it: no workspaces are hidden,
    /// the renderer only looks at this output's.
    pub(crate) fn with_render_shell<R>(&mut self, output: &Output, f: impl FnOnce(&mut Self) -> R) -> R {
        let name = output.name();
        if self.shells.scoped || self.shells.loaded.is_none() || self.shells.loaded.as_deref() == Some(name.as_str()) {
            return f(self);
        }
        let previous = self.shell_load(&name);
        let result = f(self);
        if let Some(previous) = previous {
            self.shell_load(&previous);
        }
        result
    }

    /// `with_shell` for the loaded output, the one keys and gestures act on.
    /// With a single output nothing is narrowed.
    pub(crate) fn with_loaded_shell<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        if self.shells.scoped || self.workspaces.outputs.len() <= 1 {
            return f(self);
        }
        match self.loaded_output() {
            Some(output) => self.with_shell(&output, f),
            None => f(self),
        }
    }

    /// Some output other than the loaded one is a console.
    pub(crate) fn shells_any_tty(&self) -> bool {
        self.shells.stored.iter().any(|(name, c)| c.tty_on && self.shells.loaded.as_deref() != Some(name.as_str()))
    }


    /// Runs `f` once per output, each with its own shell.
    pub(crate) fn for_each_shell(&mut self, mut f: impl FnMut(&mut Self)) {
        if self.shells.scoped || self.shells.loaded.is_none() {
            return f(self);
        }
        let outputs: Vec<Output> = self.workspaces.outputs.iter().map(|o| o.output.clone()).collect();
        if outputs.len() <= 1 {
            return f(self);
        }
        for output in outputs {
            self.with_shell(&output, &mut f);
        }
    }

    /// The settled Classic mode and the TTY mode of `output`'s shell.
    pub(crate) fn shell_of(&self, output: &Output) -> (ShellMode, bool) {
        let name = output.name();
        match self.shells.stored.get(&name) {
            Some(context) if self.shells.loaded.as_deref() != Some(name.as_str()) => (context.settled, context.tty_on),
            _ => (self.classic.settled, self.tty.on),
        }
    }

    /// The settled Classic mode of the output `window` lives on.
    pub(crate) fn window_shell(&self, window: &Window) -> (ShellMode, bool) {
        // Inside a scope other outputs' workspaces are hidden, but their
        // windows are still mapped: place those by geometry.
        let output = self.workspaces.membership(window).map(|(o, _)| o)
            .or_else(|| self.space.element_location(window).and_then(|_| self.output_for_window(window)));
        match output {
            Some(output) => self.shell_of(&output),
            None => (self.classic.settled, self.tty.on),
        }
    }

    /// The pointer moved to another output: its shell takes keys and gestures.
    /// A transition still running on the old output finishes at once.
    pub(crate) fn follow_active_shell(&mut self) {
        if self.shells.scoped || self.greeter.is_some() {
            return;
        }
        let Some(active) = self.active_output() else { return };
        let name = active.name();
        // A new output starts in the mode the loaded shell is in when it appears.
        let is_unseen = |gv: &Self, n: &String| *n != name && gv.shells.loaded.as_ref() != Some(n) && !gv.shells.stored.contains_key(n);
        if self.workspaces.outputs.iter().any(|o| is_unseen(self, &o.output.name())) {
            let unseen: Vec<String> = self.workspaces.outputs.iter().map(|o| o.output.name()).filter(|n| is_unseen(self, n)).collect();
            for unseen in unseen {
                let fresh = self.fresh_shell_context();
                self.shells.stored.insert(unseen, fresh);
            }
            if self.shells.loaded.is_some() {
                self.restore_output_modes();
            }
        }
        if self.shells.loaded.as_deref() == Some(name.as_str()) {
            return;
        }
        if self.shells.loaded.is_some() {
            self.classic.held = false;
            let target = self.classic.mode.progress();
            self.classic.progress.jump(target);
            self.tty.held = false;
            self.tty.progress.jump(if self.tty.on { 1.0 } else { 0.0 });
            // Window effects of a Classic switch still belong to the old output.
            self.settle_classic_here();
        }
        self.shell_load(&name);
        self.sync_tty_flags();
        if self.keyboard_first() { self.hide_pointer(); } else { self.show_pointer(); }
        self.restore_output_modes();
        self.queue_redraw_all();
    }

    /// Settles every output whose Classic switch finished animating.
    pub fn settle_classic(&mut self) {
        self.settle_classic_here();
        if self.shells.stored.values().any(ShellContext::unsettled) {
            let outputs: Vec<Output> = self.workspaces.outputs.iter().map(|o| o.output.clone())
                .filter(|o| self.shells.stored.get(&o.name()).is_some_and(ShellContext::unsettled)).collect();
            for output in outputs {
                self.with_shell(&output, |gv| gv.settle_classic_here());
            }
        }
    }

    /// Every output's mode, by connector, for the state file.
    pub(crate) fn output_modes(&self) -> Vec<(String, Mode)> {
        let mut modes: Vec<(String, Mode)> = self.shells.stored.iter().map(|(n, c)| (n.clone(), c.mode())).collect();
        if let Some(loaded) = &self.shells.loaded {
            modes.push((loaded.clone(), self.current_mode()));
        }
        for (name, mode) in &self.shells.restore {
            if !modes.iter().any(|(n, _)| n == name) {
                modes.push((name.clone(), *mode));
            }
        }
        modes.sort();
        modes
    }

    /// The first time an output shows up, it takes the mode it had in the
    /// last session.
    pub(crate) fn restore_output_modes(&mut self) {
        if self.shells.restore.is_empty() || self.shells.scoped {
            return;
        }
        let outputs: Vec<Output> = self.workspaces.outputs.iter().map(|o| o.output.clone())
            .filter(|o| self.shells.restore.contains_key(&o.name())).collect();
        for output in outputs {
            let Some(mode) = self.shells.restore.remove(&output.name()) else { continue };
            self.restore_output_mode(&output, mode);
        }
    }

    fn restore_output_mode(&mut self, output: &Output, mode: Mode) {
        if self.config.classic.start_in_classic || self.config.tty.start {
            return;
        }
        let (classic, tty) = self.shell_of(output);
        let now = if classic == ShellMode::Classic { Mode::Classic } else if tty { Mode::Tty } else { Mode::Gavno };
        if now == mode {
            return;
        }
        self.with_shell(output, |gv| gv.switch_mode_now(mode));
    }

    /// Puts the loaded shell into `mode` without animation.
    pub(crate) fn switch_mode_now(&mut self, mode: Mode) {
        match mode {
            Mode::Classic => {
                if self.features.on(crate::module::Feature::Classic) {
                    self.set_shell_mode(ShellMode::Classic);
                    self.classic.progress.jump(1.0);
                    self.settle_classic_here();
                }
            }
            Mode::Tty | Mode::Gavno => {
                if self.classic.settled == ShellMode::Classic || self.classic.mode == ShellMode::Classic {
                    self.set_shell_mode(ShellMode::Gavno);
                    self.classic.progress.jump(0.0);
                    self.settle_classic_here();
                }
                let tty = mode == Mode::Tty;
                if self.tty.on != tty {
                    self.set_tty_mode(tty);
                    self.tty.progress.jump(if tty { 1.0 } else { 0.0 });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::Harness;

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
            Some(smithay::output::Mode { size: (1600, 900).into(), refresh: 60_000 }),
            None,
            None,
            Some((x, 0).into()),
        );
        gv.space.map_output(&o, (x, 0));
        gv.refresh_workspaces();
        o
    }

    fn point_at(gv: &mut Gavno, x: f64) {
        gv.warp_pointer((x, 400.0).into());
        gv.follow_active_shell();
    }

    #[test]
    fn each_output_keeps_its_own_mode() {
        let mut h = Harness::new();
        h.gv.tty.animate = Some(false);
        let left = output(&mut h.gv, "left", 0);
        let right = output(&mut h.gv, "right", 1600);
        point_at(&mut h.gv, 2000.0);
        assert_eq!(h.gv.shells.loaded(), Some("right"));
        h.gv.set_tty_mode(true);
        assert!(h.gv.tty.on);
        assert_eq!(h.gv.shell_of(&right), (ShellMode::Gavno, true));
        assert_eq!(h.gv.shell_of(&left), (ShellMode::Gavno, false));
        // Only the right output's workspaces were forced into tiling.
        assert!(h.gv.workspaces.get(&right).unwrap().list.items.iter().all(|w| w.tiling));
        assert!(h.gv.workspaces.get(&left).unwrap().list.items.iter().all(|w| !w.tiling));
        point_at(&mut h.gv, 100.0);
        assert_eq!(h.gv.shells.loaded(), Some("left"));
        assert!(!h.gv.tty.on);
        assert_eq!(h.gv.shell_of(&right), (ShellMode::Gavno, true));
        // The work area follows each output's own shell.
        let tty_area = h.gv.work_area(&right);
        assert_eq!(tty_area.loc.y, crate::tty_mode::BAR_HEIGHT);
        // Back on the right output the console is still there.
        point_at(&mut h.gv, 2000.0);
        assert!(h.gv.tty.on);
        assert_eq!(h.gv.workspaces.outputs.len(), 2);
    }

    #[test]
    fn classic_on_one_output_leaves_the_other_alone() {
        let mut h = Harness::new();
        let left = output(&mut h.gv, "left", 0);
        let right = output(&mut h.gv, "right", 1600);
        point_at(&mut h.gv, 100.0);
        h.gv.switch_mode_now(Mode::Classic);
        assert_eq!(h.gv.shell_of(&left), (ShellMode::Classic, false));
        assert_eq!(h.gv.shell_of(&right), (ShellMode::Gavno, false));
        let left_area = h.gv.work_area(&left);
        let right_area = h.gv.work_area(&right);
        assert_eq!(left_area.size.h, 900 - crate::classic::TASKBAR_HEIGHT);
        assert_ne!(left_area.size.h, right_area.size.h);
        point_at(&mut h.gv, 2000.0);
        assert!(!h.gv.classic_mode());
        h.gv.settle_classic();
        assert_eq!(h.gv.shell_of(&left), (ShellMode::Classic, false));
    }

    #[test]
    fn a_new_output_starts_in_the_mode_of_the_pointer_output_or_the_restored_one() {
        let mut h = Harness::new();
        h.gv.tty.animate = Some(false);
        let _left = output(&mut h.gv, "left", 0);
        point_at(&mut h.gv, 100.0);
        h.gv.set_tty_mode(true);
        h.gv.shells.restore.insert("third".into(), Mode::Gavno);
        let right = output(&mut h.gv, "right", 1600);
        let third = output(&mut h.gv, "third", 3200);
        point_at(&mut h.gv, 2000.0);
        assert_eq!(h.gv.shell_of(&right), (ShellMode::Gavno, true));
        point_at(&mut h.gv, 3300.0);
        assert_eq!(h.gv.shell_of(&third), (ShellMode::Gavno, false));
        let modes = h.gv.output_modes();
        assert_eq!(modes, vec![("left".into(), Mode::Tty), ("right".into(), Mode::Tty), ("third".into(), Mode::Gavno)]);
    }
}
