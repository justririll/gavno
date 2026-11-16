//! Panic diagnostics, including panics recovered at optional feature boundaries.
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

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
    let mut file = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    if file.metadata()?.len() > LIMIT { file.set_len(0)?; }
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
        if let Some(path) = log_path(|key| std::env::var_os(key)) {
            if let Err(error) = append(&path, &message) {
                tracing::error!(%error, "cannot write crash log");
            }
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
        fs::remove_dir_all(root).unwrap();
    }
}
