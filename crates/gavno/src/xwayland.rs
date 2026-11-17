//! Optional xwayland-satellite supervision. Exit monitoring blocks on a child,
//! then wakes calloop through a channel; there is no polling timer.
//!
//! GAVNO owns the X display: it takes the lock file and listens on the
//! pathname and abstract sockets itself, so DISPLAY can be published at once.
//! The satellite (and the Xwayland behind it) is started lazily: the listening
//! sockets sit in the event loop, and the first X11 client to connect starts
//! the satellite, which receives the sockets with `-listenfd` and accepts the
//! connection that waited in the backlog. When the satellite exits the sockets
//! are watched again, so the next client starts it anew.
use crate::Gavno;
use smithay::reexports::calloop::{
    channel::{self, Event},
    generic::Generic,
    Interest, Mode, PostAction, RegistrationToken,
};
use std::{
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    rc::Rc,
    time::{Duration, Instant},
};

/// Satellite exits within `CRASH_WINDOW` after which X11 is given up.
const CRASH_LIMIT: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(60);

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

/// A started satellite: its pidfd and the channel that reports its exit.
struct Launched {
    process: OwnedFd,
    exit: channel::Channel<std::io::Result<ExitStatus>>,
}
/// Starts the satellite for a display; replaced in tests so they never run it.
type Launcher = Rc<dyn Fn(&Gavno, &Path, &XDisplay) -> Option<Launched>>;

pub struct Satellite {
    executable: PathBuf,
    launcher: Launcher,
    /// The running satellite, if any.
    process: Option<OwnedFd>,
    watcher: Option<RegistrationToken>,
    /// Read sources on the listening sockets while no satellite runs.
    listeners: Vec<RegistrationToken>,
    exits: Vec<Instant>,
    display: Option<XDisplay>,
}
impl Drop for Satellite {
    fn drop(&mut self) {
        let Some(process) = &self.process else { return };
        // A pidfd cannot accidentally signal a recycled PID.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                process.as_raw_fd(),
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
    let name = format!(":{}", display.number);
    std::env::set_var("DISPLAY", &name);
    tracing::info!(display = %name, "X11 display ready; xwayland-satellite starts on the first X11 client");
    gv.satellite = Some(Satellite {
        executable,
        launcher: Rc::new(launch),
        process: None,
        watcher: None,
        listeners: Vec::new(),
        exits: Vec::new(),
        display: Some(display),
    });
    arm(gv);
}

/// Watches the listening sockets; the first readable one starts the satellite.
/// The sources are level-triggered and never accept, so the connection stays
/// queued for the satellite.
fn arm(gv: &mut Gavno) {
    let Some(satellite) = gv.satellite.as_mut() else { return };
    let Some(display) = &satellite.display else { return };
    let mut tokens = Vec::new();
    for fd in [&display.unix, &display.abstract_] {
        // A dup shares the open socket; the display keeps the original.
        let Ok(fd) = fd.try_clone() else { continue };
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        match gv.loop_handle.insert_source(source, |_, _, gv| {
            connected(gv);
            Ok(PostAction::Continue)
        }) {
            Ok(token) => tokens.push(token),
            Err(err) => tracing::warn!(?err, "could not watch the X display socket"),
        }
    }
    if tokens.is_empty() {
        tracing::warn!("could not watch the X display; X11 support disabled");
        give_up(gv);
        return;
    }
    if let Some(satellite) = gv.satellite.as_mut() {
        satellite.listeners = tokens;
    }
}
fn disarm(gv: &mut Gavno) {
    if let Some(satellite) = gv.satellite.as_mut() {
        for token in std::mem::take(&mut satellite.listeners) {
            gv.loop_handle.remove(token);
        }
    }
}
/// Stops X11 for the session: no more starts, and no DISPLAY for new clients.
fn give_up(gv: &mut Gavno) {
    disarm(gv);
    gv.satellite = None;
    std::env::remove_var("DISPLAY");
}

/// An X11 client connected: start the satellite.
fn connected(gv: &mut Gavno) {
    let _stall = crate::perf::Stall::new("xwayland.connect");
    // Both sockets may be readable in one dispatch; only the first starts it.
    if gv.satellite.as_ref().is_none_or(|s| s.listeners.is_empty()) {
        return;
    }
    disarm(gv);
    let Some(satellite) = gv.satellite.as_ref() else { return };
    let Some(display) = &satellite.display else { return };
    let Some(Launched { process, exit }) = (satellite.launcher)(gv, &satellite.executable, display) else {
        // Counts as an exit, so a satellite that cannot start does not spin.
        exited(gv);
        return;
    };
    let watcher = gv.loop_handle.insert_source(exit, |event, _, gv| {
        let _stall = crate::perf::Stall::new("xwayland.exit");
        if let Event::Msg(status) = event {
            tracing::warn!(?status, "xwayland-satellite exited");
            if let Some(satellite) = gv.satellite.as_mut() {
                if let Some(watcher) = satellite.watcher.take() {
                    gv.loop_handle.remove(watcher);
                }
                satellite.process = None;
            }
            exited(gv);
        }
    });
    let Some(satellite) = gv.satellite.as_mut() else { return };
    match watcher {
        Ok(token) => {
            satellite.watcher = Some(token);
            satellite.process = Some(process);
        }
        Err(err) => {
            tracing::warn!(?err, "could not install xwayland-satellite exit watcher");
            satellite.process = Some(process); // dropped below: SIGTERM
            gv.satellite = None;
            std::env::remove_var("DISPLAY");
        }
    }
}
/// The satellite is gone: watch the sockets again for the next client, unless
/// it keeps dying.
fn exited(gv: &mut Gavno) {
    let Some(satellite) = gv.satellite.as_mut() else { return };
    let now = Instant::now();
    satellite.exits.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
    satellite.exits.push(now);
    if satellite.exits.len() >= CRASH_LIMIT {
        tracing::warn!("xwayland-satellite keeps exiting; X11 support disabled");
        give_up(gv);
    } else {
        arm(gv);
    }
}

fn launch(gv: &Gavno, executable: &Path, display: &XDisplay) -> Option<Launched> {
    let display_name = format!(":{}", display.number);
    let fds = [display.unix.as_raw_fd(), display.abstract_.as_raw_fd()];
    let mut command = Command::new(executable);
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
            return None;
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
        return None;
    }
    let process = unsafe { OwnedFd::from_raw_fd(raw as _) };
    let (sender, exit) = channel::channel();
    tracing::info!(display = %display_name, "started xwayland-satellite");
    crate::perf::startup("xwayland.started");
    std::thread::spawn(move || {
        let status = wait_child(&mut child);
        let _ = sender.send(status);
    });
    Some(Launched { process, exit })
}
fn wait_child(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    child.wait()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
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
    fn test_satellite(gv: &mut Gavno, dir: &Path, launches: Rc<std::cell::Cell<u32>>, senders: Rc<std::cell::RefCell<Vec<channel::Sender<std::io::Result<ExitStatus>>>>>) {
        let display = claim_display(dir, false).unwrap();
        let launcher: Launcher = Rc::new(move |_, _, _| {
            launches.set(launches.get() + 1);
            let (sender, exit) = channel::channel();
            senders.borrow_mut().push(sender);
            // Any valid fd stands in for the pidfd; signalling it just fails.
            let process = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
            Some(Launched { process, exit })
        });
        gv.satellite = Some(Satellite {
            executable: PathBuf::from("xwayland-satellite"),
            launcher,
            process: None,
            watcher: None,
            listeners: Vec::new(),
            exits: Vec::new(),
            display: Some(display),
        });
        arm(gv);
    }
    #[test]
    fn satellite_starts_on_first_client_and_is_rearmed_after_exit_until_crash_loop() {
        let dir = std::env::temp_dir().join(format!("gavno-x-lazy-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".X11-unix")).unwrap();
        let mut h = crate::state::tests::Harness::new();
        let launches = Rc::new(std::cell::Cell::new(0));
        let senders = Rc::new(std::cell::RefCell::new(Vec::new()));
        test_satellite(&mut h.gv, &dir, launches.clone(), senders.clone());
        let socket = dir.join(".X11-unix/X0");
        let dispatch = |h: &mut crate::state::tests::Harness| {
            for _ in 0..3 {
                h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
            }
        };
        // Idle: nothing starts without a client.
        dispatch(&mut h);
        assert_eq!(launches.get(), 0);
        // The first client starts the satellite exactly once, even though its
        // connection stays unaccepted (level-triggered sources were removed).
        let _client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        dispatch(&mut h);
        assert_eq!(launches.get(), 1);
        dispatch(&mut h);
        assert_eq!(launches.get(), 1);
        assert!(h.gv.satellite.as_ref().unwrap().listeners.is_empty());
        // An exit re-arms the watch; the still queued connection starts it again.
        senders.borrow()[0].send(Ok(ExitStatus::from_raw(0))).unwrap();
        dispatch(&mut h);
        assert_eq!(launches.get(), 2);
        // The third exit within the window gives up.
        senders.borrow()[1].send(Ok(ExitStatus::from_raw(0))).unwrap();
        dispatch(&mut h);
        assert_eq!(launches.get(), 3);
        senders.borrow()[2].send(Ok(ExitStatus::from_raw(0))).unwrap();
        dispatch(&mut h);
        assert_eq!(launches.get(), 3);
        assert!(h.gv.satellite.is_none());
        assert!(!socket.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
