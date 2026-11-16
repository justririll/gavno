//! Optional xwayland-satellite supervision. Exit monitoring blocks on a child,
//! then wakes calloop through a channel; there is no polling timer.
//!
//! GAVNO owns the X display: it takes the lock file and listens on the
//! pathname and abstract sockets itself, then hands them to the satellite
//! with `-listenfd`. Clients that connect before the X server is ready wait
//! in the listen backlog, so DISPLAY can be published at once, and the
//! display survives a satellite restart.
use crate::Gavno;
use smithay::reexports::calloop::channel::{self, Event};
use std::{
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command},
};

/// A claimed X display: lock file plus listening sockets.
struct XDisplay {
    number: u16,
    lock: PathBuf,
    path: PathBuf,
    unix: OwnedFd,
    abstract_: OwnedFd,
}
impl Drop for XDisplay {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.lock);
    }
}

pub struct Satellite {
    process: OwnedFd,
    executable: PathBuf,
    restarted: bool,
    watcher: smithay::reexports::calloop::RegistrationToken,
    display: Option<XDisplay>,
}
impl Drop for Satellite {
    fn drop(&mut self) {
        // A pidfd cannot accidentally signal a recycled PID.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.process.as_raw_fd(),
                libc::SIGTERM,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}
fn executable_in_path() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|p| p.join("xwayland-satellite"))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}
/// Include both pathname socket and lock file: a display may be starting up.
#[cfg(test)]
fn pick_display(mut occupied: impl FnMut(u16) -> bool) -> Option<u16> {
    (0..=1023).find(|&n| !occupied(n))
}
fn occupied_in(dir: &Path, n: u16) -> bool {
    // symlink_metadata also treats dangling entries as occupied.
    dir.join(format!(".X{n}-lock")).symlink_metadata().is_ok()
        || dir
            .join(format!(".X11-unix/X{n}"))
            .symlink_metadata()
            .is_ok()
}
fn abstract_socket_occupied(n: u16) -> bool {
    // Linux X servers may expose only the abstract AF_UNIX socket.
    let name = format!("/tmp/.X11-unix/X{n}");
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return true;
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as _;
        for (dst, src) in addr.sun_path[1..].iter_mut().zip(name.bytes()) {
            *dst = src as _;
        }
        let len =
            (std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len()) as libc::socklen_t;
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            len,
        ) < 0
    }
}
/// A listening AF_UNIX stream socket, abstract when `name` starts with NUL.
fn listen_unix(name: &[u8]) -> Option<OwnedFd> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return None;
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as _;
        if name.len() >= addr.sun_path.len() {
            return None;
        }
        for (dst, src) in addr.sun_path.iter_mut().zip(name) {
            *dst = *src as _;
        }
        let len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + name.len()) as libc::socklen_t;
        if libc::bind(fd.as_raw_fd(), &addr as *const _ as *const libc::sockaddr, len) < 0 {
            return None;
        }
        if libc::listen(fd.as_raw_fd(), 128) < 0 {
            return None;
        }
        Some(fd)
    }
}

/// Claims the first free display under `dir` (normally /tmp).
fn claim_display(dir: &Path, check_abstract: bool) -> Option<XDisplay> {
    let sockets = dir.join(".X11-unix");
    if std::fs::create_dir_all(&sockets).is_ok() {
        // /tmp/.X11-unix is world-writable and sticky, as X servers expect.
        if let Ok(meta) = sockets.metadata() {
            if meta.permissions().mode() & 0o7777 != 0o1777 {
                let _ = std::fs::set_permissions(&sockets, std::fs::Permissions::from_mode(0o1777));
            }
        }
    }
    for number in 0..=1023u16 {
        remove_stale_display(dir, number);
        if occupied_in(dir, number) || (check_abstract && abstract_socket_occupied(number)) {
            continue;
        }
        let lock = dir.join(format!(".X{number}-lock"));
        let Ok(mut file) = std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) else {
            continue;
        };
        // The X lock file format: the owner's pid, right aligned in 10 columns.
        if writeln!(file, "{:>10}", std::process::id()).is_err() {
            let _ = std::fs::remove_file(&lock);
            continue;
        }
        let path = sockets.join(format!("X{number}"));
        let mut abstract_name = vec![0u8];
        abstract_name.extend_from_slice(path.as_os_str().as_encoded_bytes());
        let abstract_ = listen_unix(&abstract_name);
        let unix = listen_unix(path.as_os_str().as_encoded_bytes());
        match (unix, abstract_) {
            (Some(unix), Some(abstract_)) => {
                return Some(XDisplay { number, lock, path, unix, abstract_ });
            }
            _ => {
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(&lock);
            }
        }
    }
    None
}

/// Cleans up after an X server that died without doing it itself (GAVNO
/// killed by a signal, a crashed Xwayland), like Xorg does: a lock file whose
/// pid is gone, or a socket without a lock file that refuses connections.
/// Without this every crash moves X to the next display number.
fn remove_stale_display(dir: &Path, number: u16) {
    let path = dir.join(format!(".X11-unix/X{number}"));
    let lock = dir.join(format!(".X{number}-lock"));
    if lock.symlink_metadata().is_ok() {
        let pid = std::fs::read_to_string(&lock)
            .ok()
            .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
            .filter(|&pid| pid > 0);
        let Some(pid) = pid else { return };
        // SAFETY: signal 0 only checks that the process exists.
        let dead = unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if dead && std::fs::remove_file(&lock).is_ok() {
            let _ = std::fs::remove_file(&path);
        }
        return;
    }
    if path.symlink_metadata().is_err() {
        return;
    }
    let refused = std::os::unix::net::UnixStream::connect(&path)
        .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused);
    if refused {
        let _ = std::fs::remove_file(&path);
    }
}

pub fn start(gv: &mut Gavno) {
    let _startup = crate::perf::Startup::new("xwayland.start.begin", "xwayland.start.end");
    let Some(executable) = executable_in_path() else {
        tracing::info!("xwayland-satellite is not in PATH; X11 support disabled");
        return;
    };
    let Some(display) = claim_display(Path::new("/tmp"), true) else {
        tracing::warn!("no free X display; X11 support disabled");
        return;
    };
    crate::perf::startup("xwayland.display_claimed");
    spawn(gv, executable, false, display);
}
fn spawn(gv: &mut Gavno, executable: PathBuf, restarted: bool, display: XDisplay) {
    let display_name = format!(":{}", display.number);
    let fds = [display.unix.as_raw_fd(), display.abstract_.as_raw_fd()];
    let mut command = Command::new(&executable);
    command
        .arg(&display_name)
        .args(["-listenfd", &fds[0].to_string(), "-listenfd", &fds[1].to_string()])
        .env("WAYLAND_DISPLAY", &gv.socket_name);
    // The sockets are CLOEXEC in GAVNO; let the satellite inherit them.
    unsafe {
        command.pre_exec(move || {
            for fd in fds {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(?err, "could not start xwayland-satellite");
            return;
        }
    };
    crate::perf::startup("xwayland.process_spawned");
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
    if raw < 0 {
        tracing::warn!(error = ?std::io::Error::last_os_error(), "could not supervise xwayland-satellite");
        let _ = child.kill();
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        return;
    }
    let process = unsafe { OwnedFd::from_raw_fd(raw as _) };
    let (sender, receiver) = channel::channel();
    let watcher = match gv.loop_handle.insert_source(receiver, |event, _, gv| {
        let _stall = crate::perf::Stall::new("xwayland.exit");
        if let Event::Msg(status) = event {
            if let Some(mut satellite) = gv.satellite.take() {
                gv.loop_handle.remove(satellite.watcher);
                tracing::warn!(?status, "xwayland-satellite exited");
                let display = satellite.display.take();
                match display {
                    // The display stays ours; restart the server behind it once.
                    Some(display) if !satellite.restarted => {
                        spawn(gv, satellite.executable.clone(), true, display);
                    }
                    _ => std::env::remove_var("DISPLAY"),
                }
            }
        }
    }) {
        Ok(watcher) => watcher,
        Err(err) => {
            tracing::warn!(?err, "could not install xwayland-satellite exit watcher");
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return;
        }
    };
    gv.satellite = Some(Satellite {
        process,
        executable,
        restarted,
        watcher,
        display: Some(display),
    });
    std::env::set_var("DISPLAY", &display_name);
    tracing::info!(display = %display_name, "started xwayland-satellite");
    crate::perf::startup("xwayland.started");
    std::thread::spawn(move || {
        let status = wait_child(&mut child);
        let _ = sender.send(status);
    });
}
fn wait_child(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    child.wait()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn picks_first_free_display_and_handles_exhaustion() {
        assert_eq!(pick_display(|_| false), Some(0));
        assert_eq!(pick_display(|n| n < 8 || n == 9), Some(8));
        assert_eq!(pick_display(|_| true), None);
        assert_eq!(pick_display(|n| n != 1023), Some(1023));
    }
    #[test]
    fn claims_lock_file_and_listening_socket_then_cleans_up() {
        let dir = std::env::temp_dir().join(format!("gavno-x-claim-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".X11-unix")).unwrap();
        std::fs::write(dir.join(".X0-lock"), "1").unwrap();
        // Abstract sockets are global, so do not check them in a unit test.
        let display = claim_display(&dir, false).unwrap();
        assert_eq!(display.number, 1);
        let lock = std::fs::read_to_string(dir.join(".X1-lock")).unwrap();
        assert_eq!(lock.trim().parse::<u32>().unwrap(), std::process::id());
        // Connecting succeeds before any X server runs: it waits in the backlog.
        let client = std::os::unix::net::UnixStream::connect(dir.join(".X11-unix/X1"));
        assert!(client.is_ok());
        drop(display);
        assert!(!dir.join(".X1-lock").exists());
        assert!(!dir.join(".X11-unix/X1").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn stale_displays_are_reclaimed_but_live_ones_are_not() {
        let dir = std::env::temp_dir().join(format!("gavno-x-stale-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".X11-unix")).unwrap();
        // :0 dead socket, no lock: bound once, listener dropped.
        drop(std::os::unix::net::UnixListener::bind(dir.join(".X11-unix/X0")).unwrap());
        // :1 live socket, no lock.
        let _live = std::os::unix::net::UnixListener::bind(dir.join(".X11-unix/X1")).unwrap();
        // :2 lock of a process that has exited, with its socket.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        std::fs::write(dir.join(".X2-lock"), format!("{dead:>10}\n")).unwrap();
        drop(std::os::unix::net::UnixListener::bind(dir.join(".X11-unix/X2")).unwrap());
        // :3 lock of a live process.
        std::fs::write(dir.join(".X3-lock"), format!("{:>10}\n", std::process::id())).unwrap();
        for n in 0..4 {
            remove_stale_display(&dir, n);
        }
        assert!(!dir.join(".X11-unix/X0").exists());
        assert!(dir.join(".X11-unix/X1").exists());
        assert!(!dir.join(".X2-lock").exists());
        assert!(!dir.join(".X11-unix/X2").exists());
        assert!(dir.join(".X3-lock").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn socket_and_lock_file_both_reserve_display() {
        let dir = std::env::temp_dir().join(format!("gavno-x-display-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".X11-unix")).unwrap();
        std::fs::write(dir.join(".X0-lock"), "123").unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(dir.join(".X11-unix/X1")).unwrap();
        assert_eq!(pick_display(|n| occupied_in(&dir, n)), Some(2));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
