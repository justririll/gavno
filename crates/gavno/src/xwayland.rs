//! Optional xwayland-satellite supervision. Exit monitoring blocks on a child,
//! then wakes calloop through a channel; there is no polling timer.
use crate::Gavno;
use smithay::reexports::calloop::channel::{self, Event};
use std::{
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::PermissionsExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command},
};

pub struct Satellite {
    process: OwnedFd,
    executable: PathBuf,
    restarted: bool,
    watcher: smithay::reexports::calloop::RegistrationToken,
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
pub fn start(gv: &mut Gavno) {
    let Some(executable) = executable_in_path() else {
        tracing::info!("xwayland-satellite is not in PATH; X11 support disabled");
        return;
    };
    spawn(gv, executable, false);
}
fn spawn(gv: &mut Gavno, executable: PathBuf, restarted: bool) {
    let Some(display) =
        pick_display(|n| occupied_in(Path::new("/tmp"), n) || abstract_socket_occupied(n))
    else {
        tracing::warn!("no free X display; X11 support disabled");
        return;
    };
    let display_name = format!(":{display}");
    let mut child = match Command::new(&executable)
        .arg(&display_name)
        .env("WAYLAND_DISPLAY", &gv.socket_name)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(?err, "could not start xwayland-satellite");
            return;
        }
    };
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
        if let Event::Msg(status) = event {
            if let Some(satellite) = gv.satellite.take() {
                gv.loop_handle.remove(satellite.watcher);
                tracing::warn!(?status, "xwayland-satellite exited");
                std::env::remove_var("DISPLAY");
                if !satellite.restarted {
                    spawn(gv, satellite.executable.clone(), true);
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
    });
    std::env::set_var("DISPLAY", &display_name);
    tracing::info!(display = %display_name, "started xwayland-satellite");
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
    fn socket_and_lock_file_both_reserve_display() {
        let dir = std::env::temp_dir().join(format!("gavno-x-display-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".X11-unix")).unwrap();
        std::fs::write(dir.join(".X0-lock"), "123").unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(dir.join(".X11-unix/X1")).unwrap();
        assert_eq!(pick_display(|n| occupied_in(&dir, n)), Some(2));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
