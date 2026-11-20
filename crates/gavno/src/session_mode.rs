//! The last shell mode (GAVNO, Classic or TTY) survives logins: it is written
//! to `$XDG_STATE_HOME/gavno/mode` when a change settles, and read once at
//! startup. `[classic] start_in_classic = true` and `[tty] start = true` force
//! a start mode and win over the remembered one. Nothing runs between changes:
//! no timer, no polling.

use std::{cell::RefCell, path::{Path, PathBuf}};

use crate::{classic::ShellMode, Gavno};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Gavno,
    Classic,
    Tty,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Gavno => "gavno",
            Mode::Classic => "classic",
            Mode::Tty => "tty",
        }
    }
    fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "gavno" => Some(Mode::Gavno),
            "classic" => Some(Mode::Classic),
            "tty" => Some(Mode::Tty),
            _ => None,
        }
    }
}

thread_local! {
    /// Where the compositor of this thread persists the mode; unset in tests.
    static PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// The mode read at startup, taken by `Gavno::new`.
    static RESTORE: RefCell<Option<Mode>> = const { RefCell::new(None) };
}

/// `$XDG_STATE_HOME/gavno/mode`, falling back to `~/.local/state`.
pub fn state_path() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .map(|p| p.join("gavno/mode"))
}

pub fn load(path: &Path) -> Option<Mode> {
    Mode::parse(&std::fs::read_to_string(path).ok()?)
}

/// Writes through a temporary file, so a crash never leaves half a file.
pub fn save(path: &Path, mode: Mode) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{}\n", mode.name()))?;
    std::fs::rename(&tmp, path)
}

/// Turns persistence on for this thread and remembers what was stored.
pub fn enable(path: Option<PathBuf>) {
    let stored = path.as_deref().and_then(load);
    RESTORE.with(|r| *r.borrow_mut() = stored);
    PATH.with(|p| *p.borrow_mut() = path);
}

pub(crate) fn take_restore() -> Option<Mode> {
    RESTORE.with(|r| r.borrow_mut().take())
}

/// The start mode: a forced one from the configuration, else the remembered one.
pub(crate) fn start_mode(classic_forced: bool, tty_forced: bool, restored: Option<Mode>) -> Mode {
    if classic_forced {
        Mode::Classic
    } else if tty_forced {
        Mode::Tty
    } else {
        restored.unwrap_or(Mode::Gavno)
    }
}

impl Gavno {
    /// The mode the session is in now.
    pub(crate) fn current_mode(&self) -> Mode {
        if self.classic.settled == ShellMode::Classic {
            Mode::Classic
        } else if self.tty.active() {
            Mode::Tty
        } else {
            Mode::Gavno
        }
    }

    /// Called when a mode change settled; writes only when it differs from
    /// what is stored.
    pub(crate) fn persist_mode(&mut self) {
        let mode = self.current_mode();
        if self.persisted_mode == Some(mode) {
            return;
        }
        self.persisted_mode = Some(mode);
        PATH.with(|path| {
            if let Some(path) = path.borrow().as_ref() {
                if let Err(error) = save(path, mode) {
                    tracing::warn!(%error, "cannot save the shell mode");
                }
            }
        });
    }
}

#[cfg(test)]
pub(crate) fn set_path_for_test(path: Option<PathBuf>) {
    PATH.with(|p| *p.borrow_mut() = path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_through_the_file_and_damage_reads_as_none() {
        let dir = std::env::temp_dir().join(format!("gavno-mode-{}", std::process::id()));
        let path = dir.join("gavno/mode");
        assert_eq!(load(&path), None);
        for mode in [Mode::Tty, Mode::Classic, Mode::Gavno] {
            save(&path, mode).unwrap();
            assert_eq!(load(&path), Some(mode));
        }
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(load(&path), None);
        assert!(!path.with_extension("tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn configuration_forces_a_start_mode_over_the_remembered_one() {
        assert_eq!(start_mode(false, false, Some(Mode::Tty)), Mode::Tty);
        assert_eq!(start_mode(false, false, Some(Mode::Classic)), Mode::Classic);
        assert_eq!(start_mode(false, false, None), Mode::Gavno);
        assert_eq!(start_mode(false, true, Some(Mode::Classic)), Mode::Tty);
        assert_eq!(start_mode(true, true, Some(Mode::Tty)), Mode::Classic);
        assert_eq!(start_mode(true, false, Some(Mode::Gavno)), Mode::Classic);
    }

    #[test]
    fn a_settled_change_is_written_once_and_restored_at_startup() {
        let dir = std::env::temp_dir().join(format!("gavno-mode-session-{}", std::process::id()));
        let path = dir.join("mode");
        let _ = std::fs::remove_dir_all(&dir);
        let mut h = crate::state::tests::Harness::new();
        h.gv.config.tty.autostart_terminal = false;
        h.gv.tty.animate = Some(false);
        set_path_for_test(Some(path.clone()));
        h.gv.set_tty_mode(true);
        assert_eq!(load(&path), Some(Mode::Tty));
        let first = std::fs::metadata(&path).unwrap().modified().unwrap();
        h.gv.persist_mode();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), first, "unchanged mode is not rewritten");
        h.gv.set_tty_mode(false);
        assert_eq!(load(&path), Some(Mode::Gavno));
        // Startup: the remembered mode is taken once.
        enable(Some(path.clone()));
        save(&path, Mode::Tty).unwrap();
        enable(Some(path.clone()));
        assert_eq!(take_restore(), Some(Mode::Tty));
        assert_eq!(take_restore(), None);
        let mut event_loop = smithay::reexports::calloop::EventLoop::try_new().unwrap();
        let display = smithay::reexports::wayland_server::Display::new().unwrap();
        RESTORE.with(|r| *r.borrow_mut() = Some(Mode::Tty));
        let gv = Gavno::new(&mut event_loop, display, crate::config::Config::build(true, None, |_| None).0);
        assert!(gv.tty.on, "restored");
        assert_eq!(gv.persisted_mode, Some(Mode::Tty));
        let mut event_loop = smithay::reexports::calloop::EventLoop::try_new().unwrap();
        let display = smithay::reexports::wayland_server::Display::new().unwrap();
        RESTORE.with(|r| *r.borrow_mut() = Some(Mode::Tty));
        let config = crate::config::Config::parse_checked(true, "[classic]\nstart_in_classic = true", |_| None).unwrap();
        let gv = Gavno::new(&mut event_loop, display, config);
        assert!(!gv.tty.on && gv.classic.mode == ShellMode::Classic, "a forced start mode wins");
        set_path_for_test(None);
        RESTORE.with(|r| *r.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
