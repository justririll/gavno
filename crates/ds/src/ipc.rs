//! Local window management IPC. No rendering/backend work runs outside calloop.
mod protocol;
mod transport;

use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use smithay::{
    desktop::Window,
    utils::IsAlive,
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

use crate::Ds;

struct WindowId(u64);

#[derive(Default)]
pub struct IpcState {
    last_id: u64,
    socket: Option<SocketPath>,
}

impl IpcState {
    pub fn assign_id(&mut self, window: &Window) {
        if window.user_data().get::<WindowId>().is_none() {
            self.last_id = self.last_id.checked_add(1).expect("window id exhausted");
            window
                .user_data()
                .insert_if_missing(|| WindowId(self.last_id));
        }
    }
}

fn window_id(window: &Window) -> Option<u64> {
    window.user_data().get::<WindowId>().map(|id| id.0)
}

// Remove only the socket we created, including when listener registration fails.
struct SocketPath {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path) {
            if meta.dev() == self.device && meta.ino() == self.inode {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

fn bind(path: &Path) -> io::Result<(UnixListener, SocketPath)> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("IPC socket needs a directory"))?;
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    if !fs::symlink_metadata(dir)?.file_type().is_dir() {
        return Err(io::Error::other("IPC directory must be a real directory"));
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                return Err(io::Error::other("IPC path exists and is not a socket"));
            }
            match UnixStream::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "ds IPC socket is already active",
                    ))
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(path)?
                }
                Err(e) => return Err(e),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    let meta = fs::symlink_metadata(path)?;
    let guard = SocketPath {
        path: path.to_owned(),
        device: meta.dev(),
        inode: meta.ino(),
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok((listener, guard))
}

pub fn init(ds: &mut Ds) -> io::Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is required for ds IPC"))?;
    let path = PathBuf::from(runtime).join("ds/ipc.sock");
    let (listener, socket) = bind(&path)?;
    transport::listen(ds.loop_handle.clone(), listener, |line, ds| {
        protocol::reply(line, |method, params| dispatch(ds, method, params))
    })?;
    ds.ipc.socket = Some(socket);
    std::env::set_var("DS_SOCK", &path);
    tracing::info!(?path, "ds IPC is listening");
    Ok(())
}

fn screen(ds: &Ds) -> Value {
    let focused = ds.focused_window().as_ref().and_then(window_id);
    let outputs: Vec<_> = ds
        .space
        .outputs()
        .filter_map(|output| {
            let geo = ds.space.output_geometry(output)?;
            Some(
                json!({"name": output.name(), "x": geo.loc.x, "y": geo.loc.y,
            "w": geo.size.w, "h": geo.size.h, "scale": output.current_scale().fractional_scale()}),
            )
        })
        .collect();
    let windows: Vec<_> = ds
        .space
        .elements()
        .rev()
        .filter(|w| w.alive())
        .filter_map(|window| {
            let id = window_id(window)?;
            let toplevel = window.toplevel()?;
            let geo = ds.space.element_geometry(window)?;
            let (app_id, title) = with_states(toplevel.wl_surface(), |states| {
                let attributes = states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap();
                (
                    attributes.app_id.clone().unwrap_or_default(),
                    attributes.title.clone().unwrap_or_default(),
                )
            });
            // Derive from current geometry: Space's output membership refreshes on idle.
            let output = ds
                .space
                .outputs()
                .find(|output| {
                    ds.space
                        .output_geometry(output)
                        .is_some_and(|area| area.overlaps(geo))
                })
                .map(|output| output.name());
            Some(json!({"id": id, "app_id": app_id, "title": title,
            "x": geo.loc.x, "y": geo.loc.y, "w": geo.size.w, "h": geo.size.h,
            "focused": focused == Some(id), "output": output}))
        })
        .collect();
    json!({"outputs": outputs, "windows": windows, "focused": focused})
}

fn dispatch(ds: &mut Ds, method: &str, params: &Value) -> Result<Value, String> {
    if method == "screen" {
        return Ok(screen(ds));
    }
    if !matches!(method, "focus" | "close" | "move" | "raise") {
        return Err(format!("unknown method: {method}"));
    }
    let id = params
        .get("id")
        .and_then(Value::as_u64)
        .ok_or("id must be a non-negative integer")?;
    // Validate everything before any side effect.
    let movement = if method == "move" {
        Some((
            protocol::integer(params, "x")?,
            protocol::integer(params, "y")?,
            protocol::dimension(params, "w")?,
            protocol::dimension(params, "h")?,
        ))
    } else {
        None
    };
    audit(method, params).map_err(|e| format!("cannot append agent audit log: {e}"))?;
    let window = ds
        .space
        .elements()
        .find(|w| window_id(w) == Some(id) && w.alive())
        .cloned()
        .ok_or_else(|| format!("unknown window id: {id}"))?;
    match method {
        "focus" => ds.focus_window(Some(&window)),
        "close" => window
            .toplevel()
            .ok_or("window has no toplevel")?
            .send_close(),
        "raise" => ds.space.raise_element(&window, false),
        "move" => {
            let (x, y, w, h) = movement.unwrap();
            if w.is_some() || h.is_some() {
                let size = window.geometry().size;
                let toplevel = window.toplevel().ok_or("window has no toplevel")?;
                toplevel.with_pending_state(|state| {
                    state.size =
                        Some((w.unwrap_or(size.w).max(1), h.unwrap_or(size.h).max(1)).into());
                });
                toplevel.send_pending_configure();
            }
            ds.space.map_element(window, (x, y), false);
        }
        _ => unreachable!(),
    }
    ds.queue_redraw_all();
    Ok(json!({"ok": true}))
}

fn audit(method: &str, params: &Value) -> io::Result<()> {
    let home = std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| io::Error::other("HOME is unset"))?;
    append_audit(
        &PathBuf::from(home).join(".local/state/ds/agent.log"),
        method,
        params,
    )
}

fn append_audit(path: &Path, method: &str, params: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs();
    let line = format!("{now} {method} {params}\n");
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ds-ipc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn replaces_stale_socket_and_cleans_up_without_unlinking_active_socket() {
        let root = temp_dir();
        let path = root.join("ds/ipc.sock");
        fs::create_dir(root.join("ds")).unwrap();
        drop(UnixListener::bind(&path).unwrap());
        let (listener, guard) = bind(&path).unwrap();
        assert_eq!(
            fs::metadata(root.join("ds")).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(bind(&path).is_err());
        drop(listener);
        drop(guard);
        assert!(!path.exists());
        fs::write(&path, "keep").unwrap();
        assert!(bind(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audit_appends_single_lines_with_escaped_params() {
        let root = temp_dir();
        let path = root.join("state/ds/agent.log");
        let params = json!({"id": 3, "extra": "line\nbreak"});
        append_audit(&path, "focus", &params).unwrap();
        append_audit(&path, "raise", &json!({"id": 3})).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let fields: Vec<_> = lines[0].splitn(3, ' ').collect();
        assert!(fields[0].parse::<u64>().unwrap() > 0);
        assert_eq!(fields[1], "focus");
        assert_eq!(serde_json::from_str::<Value>(fields[2]).unwrap(), params);
        fs::remove_dir_all(root).unwrap();
    }
}
