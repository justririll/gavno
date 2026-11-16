//! Root mode: a gavno-files-helper started through pkexec that performs
//! file operations for the window. Closing the session closes the helper's
//! stdin, which ends it (we cannot signal a root process).
use crate::root_proto::{Entry, Reply, Request};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
};

const HELPER: &str = "/usr/local/libexec/gavno-files-helper";

struct Pipes {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub struct RootSession {
    pipes: Arc<Mutex<Option<Pipes>>>,
    child: Arc<Mutex<Option<Child>>>,
}

fn helper_path() -> String {
    std::env::var("GAVNO_FILES_HELPER")
        .ok()
        .filter(|p| Path::new(p).is_absolute())
        .unwrap_or_else(|| HELPER.into())
}

impl RootSession {
    /// Starts the helper; resolves once polkit has authenticated the user.
    pub async fn start(app: &adw::Application) -> Result<RootSession, String> {
        crate::polkit::ensure(app)?;
        let helper = helper_path();
        if !Path::new(&helper).exists() {
            return Err(format!("{helper} is not installed"));
        }
        let mut child = Command::new("pkexec")
            .arg(&helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot start pkexec: {e}"))?;
        let pipes = Pipes {
            stdin: child.stdin.take().ok_or("no helper stdin")?,
            stdout: BufReader::new(child.stdout.take().ok_or("no helper stdout")?),
        };
        let session = RootSession {
            pipes: Arc::new(Mutex::new(Some(pipes))),
            child: Arc::new(Mutex::new(Some(child))),
        };
        match session.call(Request::Ping).await {
            Ok(_) => Ok(session),
            Err(_) => {
                let reason = session.failure_reason();
                Err(reason)
            }
        }
    }

    /// Why pkexec exited: cancelled, refused or something else.
    fn failure_reason(&self) -> String {
        let child = self.child.lock().ok().and_then(|mut c| c.take());
        self.pipes.lock().ok().map(|mut p| p.take());
        let Some(child) = child else { return "Root mode is unavailable".into() };
        let output = child.wait_with_output();
        match output {
            Ok(out) => match out.status.code() {
                Some(126) => "Authentication was cancelled".into(),
                Some(127) => "Not authorized".into(),
                _ => {
                    let text = String::from_utf8_lossy(&out.stderr).trim().to_owned();
                    if text.is_empty() { "Root mode failed to start".into() } else { text }
                }
            },
            Err(e) => e.to_string(),
        }
    }

    pub async fn call(&self, request: Request) -> Result<Reply, String> {
        let pipes = self.pipes.clone();
        gio::spawn_blocking(move || {
            let mut guard = pipes.lock().map_err(|_| "helper lock poisoned".to_string())?;
            let pipes = guard.as_mut().ok_or("root mode has ended")?;
            let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
            line.push('\n');
            pipes.stdin.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
            pipes.stdin.flush().map_err(|e| e.to_string())?;
            let mut answer = String::new();
            if pipes.stdout.read_line(&mut answer).map_err(|e| e.to_string())? == 0 {
                guard.take();
                return Err("the root helper exited".into());
            }
            serde_json::from_str::<Reply>(&answer).map_err(|e| e.to_string())
        })
        .await
        .map_err(|_| "root helper worker failed".to_string())?
    }

    /// Runs an operation that answers `Ok`.
    pub async fn run(&self, request: Request) -> Result<(), String> {
        match self.call(request).await? {
            Reply::Ok => Ok(()),
            Reply::Error(e) => Err(e),
            Reply::Entries(_) => Err("unexpected reply".into()),
        }
    }

    pub async fn list(&self, path: &Path) -> Result<Vec<Entry>, String> {
        match self.call(Request::List { path: path.to_path_buf() }).await? {
            Reply::Entries(entries) => Ok(entries),
            Reply::Error(e) => Err(e),
            Reply::Ok => Err("unexpected reply".into()),
        }
    }
}

impl Drop for RootSession {
    fn drop(&mut self) {
        // EOF on stdin ends the helper; reap pkexec off the main thread.
        if let Ok(mut pipes) = self.pipes.lock() {
            pipes.take();
        }
        if let Some(mut child) = self.child.lock().ok().and_then(|mut c| c.take()) {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// A `FileInfo` for a helper entry, shaped like the ones GIO enumerates.
pub fn file_info(dir: &Path, entry: &Entry) -> gio::FileInfo {
    let info = gio::FileInfo::new();
    info.set_name(&entry.name);
    info.set_display_name(&entry.name);
    info.set_is_hidden(entry.name.starts_with('.'));
    info.set_is_symlink(entry.symlink);
    info.set_file_type(if entry.dir { gio::FileType::Directory } else { gio::FileType::Regular });
    info.set_size(entry.size as i64);
    info.set_attribute_uint64("time::modified", entry.mtime.max(0) as u64);
    info.set_attribute_uint32("unix::mode", entry.mode);
    info.set_attribute_uint32("unix::uid", entry.uid);
    let content_type = if entry.dir {
        glib::GString::from("inode/directory")
    } else {
        gio::content_type_guess(Some(entry.name.as_str()), None::<&[u8]>).0
    };
    info.set_icon(&gio::content_type_get_icon(&content_type));
    info.set_content_type(&content_type);
    crate::model::attach(&info, &gio::File::for_path(dir.join(&entry.name)));
    info
}
