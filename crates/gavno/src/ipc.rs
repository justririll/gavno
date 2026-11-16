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

use crate::Gavno;

struct WindowId(u64);

#[derive(Default)]
pub struct IpcState {
    last_id: u64,
    socket: Option<SocketPath>,
    listener: Option<transport::Listener>,
}

impl IpcState {
    pub(crate) fn close(&mut self, handle: &smithay::reexports::calloop::LoopHandle<'static, Gavno>) {
        if let Some(listener) = self.listener.take() { handle.remove(listener.token); }
        self.socket = None;
    }
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
                        "gavno IPC socket is already active",
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

pub fn init(gv: &mut Gavno) -> io::Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is required for gavno IPC"))?;
    let path = PathBuf::from(runtime).join("gavno/ipc.sock");
    let (listener, socket) = bind(&path)?;
    let listener = transport::listen(gv.loop_handle.clone(), listener, |line, gv| request(gv, line))?;
    gv.ipc.listener = Some(listener);
    gv.ipc.socket = Some(socket);
    std::env::set_var("GAVNO_SOCK", &path);
    tracing::info!(?path, "gavno IPC is listening");
    Ok(())
}

fn request(gv: &mut Gavno, line: &[u8]) -> Vec<u8> {
    gv.guard(crate::module::Feature::Ipc, Vec::new(), |gv| protocol::reply(line, |method, params| dispatch(gv, method, params)))
}

fn screen(gv: &Gavno) -> Value {
    if gv.protocols.locks.locked { return json!({"locked": true}); }
    let focused = gv.focused_window().as_ref().and_then(window_id);
    let outputs: Vec<_> = gv
        .space
        .outputs()
        .filter_map(|output| {
            let geo = gv.space.output_geometry(output)?;
            Some(
                json!({"name": output.name(), "x": geo.loc.x, "y": geo.loc.y,
            "w": geo.size.w, "h": geo.size.h, "scale": output.current_scale().fractional_scale(),
            "hdr": crate::backend::udev::hdr_status(&gv.backend, output),
            "workspace": gv.workspaces.get(output).map(|o| o.list.active).unwrap_or(0),
            "workspaces": gv.workspaces.get(output).map(|o| o.list.items.len()).unwrap_or(1)}),
            )
        })
        .collect();
    let windows: Vec<_> = gv
        .space
        .elements()
        .chain(gv.workspaces.all().map(|v| &v.window).filter(|w| gv.space.element_location(w).is_none()))
        .collect::<Vec<_>>().into_iter().rev()
        .filter(|w| w.alive())
        .filter_map(|window| {
            let id = window_id(window)?;
            let toplevel = window.toplevel()?;
            let loc = gv.space.element_location(window).or_else(|| gv.workspaces.location(window))?;
            let geo = smithay::utils::Rectangle::new(loc, window.geometry().size);
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
            let output = gv.workspaces.membership(window).map(|(o, _)| o.name()).or_else(|| gv
                .space
                .outputs()
                .find(|output| {
                    gv.space
                        .output_geometry(output)
                        .is_some_and(|area| area.overlaps(geo))
                })
                .map(|output| output.name()));
            Some(json!({"id": id, "app_id": app_id, "title": title,
            "x": geo.loc.x, "y": geo.loc.y, "w": geo.size.w, "h": geo.size.h,
            "focused": focused == Some(id), "output": output, "minimized": crate::window_mode::mode_state(window).minimized,
            "workspace": gv.workspaces.membership(window).map(|(_, i)| i).unwrap_or(0)}))
        })
        .collect();
    json!({"outputs": outputs, "windows": windows, "focused": focused})
}

fn dispatch(gv: &mut Gavno, method: &str, params: &Value) -> Result<Value, String> {
    if gv.protocols.locks.locked && method != "screen" { return Err("session locked".into()); }
    if method == "screen" {
        return Ok(screen(gv));
    }
    if method == "set_hdr" {
        let output = params.get("output").and_then(Value::as_str).ok_or("output must be a connector name")?;
        let enabled = params.get("enabled").and_then(Value::as_bool).ok_or("enabled must be a boolean")?;
        audit(method, params).map_err(|e| format!("cannot append agent audit log: {e}"))?;
        crate::backend::udev::set_hdr(gv, output, enabled)?;
        return Ok(json!({"queued": true, "output": output, "enabled": enabled}));
    }
    if method == "workspace" {
        let output = if let Some(name) = params.get("output").and_then(Value::as_str) {
            gv.space.outputs().find(|o| o.name() == name).cloned()
        } else { gv.active_output() }.ok_or("unknown output")?;
        if params.get("workspace").is_some() {
            let index = protocol::integer(params, "workspace")?;
            if index < 0 || gv.workspaces.ensure(&output).list.items.len() <= index as usize { return Err("workspace index out of range".into()); }
            audit(method, params).map_err(|e| format!("cannot append agent audit log: {e}"))?;
            gv.switch_workspace(&output, index as usize);
        }
        let o = gv.workspaces.ensure(&output);
        return Ok(json!({"output": output.name(), "workspace": o.list.active, "count": o.list.items.len(),
            "ids": o.list.items.iter().map(|w| w.id).collect::<Vec<_>>()}));
    }
    if !matches!(method, "focus" | "close" | "move" | "raise" | "move_to_workspace" | "togglefloating") {
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
    let workspace_target = if method == "move_to_workspace" {
        let index = protocol::integer(params, "workspace")?;
        if index < 0 { return Err("workspace index out of range".into()); }
        Some(index as usize)
    } else { None };
    audit(method, params).map_err(|e| format!("cannot append agent audit log: {e}"))?;
    let window = gv
        .space
        .elements()
        .chain(gv.workspaces.all().map(|v| &v.window))
        .find(|w| window_id(w) == Some(id) && w.alive())
        .cloned()
        .ok_or_else(|| format!("unknown window id: {id}"))?;
    match method {
        "togglefloating" => gv.toggle_floating(&window),
        "move_to_workspace" => {
            if !gv.move_to_workspace(&window, workspace_target.unwrap_or(0)) { return Err("workspace index out of range".into()); }
        }
        "focus" => gv.focus_window(Some(&window)),
        "close" => window
            .toplevel()
            .ok_or("window has no toplevel")?
            .send_close(),
        "raise" => { if gv.space.element_location(&window).is_some() { gv.space.raise_element(&window, false); } gv.workspace_focus(&window); },
        "move" => {
            let from = gv.space.element_location(&window)
                .map(|loc| smithay::utils::Rectangle::new(loc, window.geometry().size));
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
            if gv.space.element_location(&window).is_some() {
                gv.space.map_element(window.clone(), (x, y), false);
            } else {
                for o in &mut gv.workspaces.outputs { for ws in &mut o.list.items {
                    if let Some(v) = ws.windows.iter_mut().find(|v| v.window == window) { v.location = (x, y).into(); }
                } }
            }
            if let Some(from) = from {
                crate::anim::animate_geometry(gv, &window, from);
            }
        }
        _ => unreachable!(),
    }
    gv.queue_redraw_all();
    Ok(json!({"ok": true}))
}

fn audit(method: &str, params: &Value) -> io::Result<()> {
    let home = std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| io::Error::other("HOME is unset"))?;
    append_audit(
        &PathBuf::from(home).join(".local/state/gavno/agent.log"),
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
            "gavno-ipc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn replaces_stale_socket_and_cleans_up_without_unlinking_active_socket() {
        let root = temp_dir();
        let path = root.join("gavno/ipc.sock");
        fs::create_dir(root.join("gavno")).unwrap();
        drop(UnixListener::bind(&path).unwrap());
        let (listener, guard) = bind(&path).unwrap();
        assert_eq!(
            fs::metadata(root.join("gavno")).unwrap().permissions().mode() & 0o777,
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
    fn locked_ipc_has_no_desktop_information_or_side_effects() {
        let mut h = crate::state::tests::Harness::new();
        let window = h.window(); h.gv.map_new_window(window.clone());
        h.gv.protocols.locks.locked = true;
        assert_eq!(dispatch(&mut h.gv, "screen", &json!({})).unwrap(), json!({"locked": true}));
        for method in ["focus", "close", "move", "raise", "set_hdr", "workspace", "move_to_workspace", "togglefloating", "unknown"] {
            assert_eq!(dispatch(&mut h.gv, method, &json!({})), Err("session locked".into()));
        }
    }

    #[test]
    fn audit_appends_single_lines_with_escaped_params() {
        let root = temp_dir();
        let path = root.join("state/gavno/agent.log");
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
    #[test]
    fn screen_includes_hidden_windows_and_zero_based_workspace_indices() {
        let mut h = crate::state::tests::Harness::new();
        let o = smithay::output::Output::new("test".into(), smithay::output::PhysicalProperties {
            size: (0, 0).into(), subpixel: smithay::output::Subpixel::Unknown, make: "test".into(), model: "test".into(),
        });
        o.change_current_state(Some(smithay::output::Mode { size: (1600, 900).into(), refresh: 120_000 }), None, None, Some((0, 0).into()));
        h.gv.space.map_output(&o, (0, 0));
        let w = h.window(); h.gv.map_new_window(w.clone());
        h.gv.space.map_element(w.clone(), (123, 234), false);
        assert!(h.gv.move_to_workspace(&w, 1));
        let result = screen(&h.gv);
        assert_eq!(result["windows"].as_array().unwrap().len(), 1);
        assert_eq!(result["windows"][0]["workspace"], 1);
        assert_eq!(result["windows"][0]["x"], 123);
        assert_eq!(result["windows"][0]["focused"], false);
        assert_eq!(result["outputs"][0]["workspace"], 0);
        assert_eq!(dispatch(&mut h.gv, "workspace", &json!({})).unwrap()["count"], 3);
        assert!(dispatch(&mut h.gv, "workspace", &json!({"workspace": -1})).is_err());
    }

}

#[cfg(test)]
mod feature_tests {
    use super::*;
    use std::io::Read;
    #[test]
    fn ipc_crash_closes_listener_and_gives_connected_clients_eof() {
        let mut h = crate::state::tests::Harness::new();
        let root = std::env::temp_dir().join(format!("gavno-ipc-reset-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let path = root.join("ipc.sock");
        let (listener, socket) = bind(&path).unwrap();
        h.gv.ipc.listener = Some(transport::listen(h.gv.loop_handle.clone(), listener, |line, gv| request(gv, line)).unwrap());
        h.gv.ipc.socket = Some(socket);
        let mut client = UnixStream::connect(&path).unwrap();
        client.set_nonblocking(true).unwrap();
        h.event_loop.dispatch(std::time::Duration::ZERO, &mut h.gv).unwrap();
        h.gv.features.inject_panic(crate::module::Feature::Ipc);
        client.write_all(b"{\"id\":1,\"method\":\"screen\"}\n").unwrap();
        h.event_loop.dispatch(std::time::Duration::ZERO, &mut h.gv).unwrap();
        assert_eq!(client.read(&mut [0; 16]).unwrap(), 0);
        assert!(h.gv.ipc.listener.is_none());
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
