//! Panic diagnostics, including panics recovered at optional feature boundaries.
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{sync_channel, SyncSender},
        OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

/// Number of active feature guards on any thread. A panic raised while one is
/// active will be caught, so the hook must not block the compositor on disk IO.
static GUARD_DEPTH: AtomicUsize = AtomicUsize::new(0);
static WORKER: OnceLock<SyncSender<(PathBuf, String)>> = OnceLock::new();

/// Marks a feature guard as active for the panic hook.
pub struct GuardScope(());
impl GuardScope {
    pub fn enter() -> Self {
        GUARD_DEPTH.fetch_add(1, Ordering::Relaxed);
        Self(())
    }
}
impl Drop for GuardScope {
    fn drop(&mut self) {
        GUARD_DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Background writer for recovered panics; drops entries when it falls behind.
fn worker() -> Option<&'static SyncSender<(PathBuf, String)>> {
    if let Some(tx) = WORKER.get() {
        return Some(tx);
    }
    let (tx, rx) = sync_channel::<(PathBuf, String)>(16);
    std::thread::Builder::new()
        .name("gavno-crash-log".into())
        .spawn(move || {
            for (path, message) in rx {
                if let Err(error) = append(&path, &message) {
                    tracing::error!(%error, "cannot write crash log");
                }
            }
        })
        .ok()?;
    Some(WORKER.get_or_init(|| tx))
}

const LIMIT: u64 = 1024 * 1024;

fn log_path(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    env("XDG_STATE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
        .or_else(|| env("HOME").filter(|v| !v.is_empty()).map(|v| PathBuf::from(v).join(".local/state")))
        .map(|p| p.join("gavno/crash.log"))
}

fn append(path: &Path, message: &str) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("crash log has no directory"))?;
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    // O_NONBLOCK and the regular-file check keep a FIFO or device planted at the
    // log path from blocking the writer.
    let mut file = OpenOptions::new().create(true).append(true).mode(0o600)
        .custom_flags(libc::O_NONBLOCK).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() { return Err(io::Error::other("crash log is not a regular file")); }
    if meta.len() > LIMIT { file.set_len(0)?; }
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    writeln!(file, "{}.{:03} UTC (Unix timestamp)\n{message}\n", timestamp.as_secs(), timestamp.subsec_millis())
}

pub fn install() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let message = format!("{} at {}\n{}", crate::module::panic_message(info.payload()),
            info.location().map(ToString::to_string).unwrap_or_else(|| "unknown location".into()),
            std::backtrace::Backtrace::force_capture());
        tracing::error!("{message}");
        let Some(path) = log_path(|key| std::env::var_os(key)) else { return };
        // A recovered feature panic must not stall rendering or input; an
        // uncaught one ends the process, so write it before unwinding further.
        if GUARD_DEPTH.load(Ordering::Relaxed) > 0 {
            if let Some(tx) = worker() {
                let _ = tx.try_send((path, message));
                return;
            }
        }
        if let Err(error) = append(&path, &message) {
            tracing::error!(%error, "cannot write crash log");
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolution_permissions_and_truncation() {
        assert_eq!(log_path(|key| match key { "XDG_STATE_HOME" => Some("/state".into()), "HOME" => Some("/home/user".into()), _ => None }), Some("/state/gavno/crash.log".into()));
        assert_eq!(log_path(|key| (key == "HOME").then(|| "/home/user".into())), Some("/home/user/.local/state/gavno/crash.log".into()));
        assert_eq!(log_path(|_| None), None);
        let root = std::env::temp_dir().join(format!("gavno-crash-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let path = root.join("gavno/crash.log");
        append(&path, "first panic at file.rs:1").unwrap();
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        OpenOptions::new().write(true).open(&path).unwrap().set_len(LIMIT + 1).unwrap();
        append(&path, "second panic").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("second panic"));
        assert!(!text.contains("first panic"));
        assert!(text.len() < 1024);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn fifo_at_log_path_does_not_block() {
        let root = std::env::temp_dir().join(format!("gavno-crash-fifo-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let path = root.join("gavno/crash.log");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // Without O_NONBLOCK this open would wait forever for a reader.
        assert!(append(&path, "panic").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
