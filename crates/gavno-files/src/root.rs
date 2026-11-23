//! Root mode: a gavno-files-helper started through pkexec that performs
//! file operations for the window. Closing the session closes the helper's
//! stdin, which ends it (we cannot signal a root process).
use crate::model::{FileEntry, ScanContext};
use crate::root_proto::{Entry, Reply, Request};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

const HELPER: &str = "/usr/local/libexec/gavno-files-helper";

struct Pipes {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub struct RootSession {
    pipes: Arc<Mutex<Option<Pipes>>>,
    child: Arc<Mutex<Option<Child>>>,
    stderr_text: Arc<Mutex<String>>,
    stderr_thread: Arc<Mutex<Option<JoinHandle<()>>>>,
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
        crate::polkit::ensure(app).await?;
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
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let stderr_thread = child.stderr.take().map(|mut se| {
            let text = stderr_text.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = io::Read::by_ref(&mut se).take(8192).read_to_end(&mut buf);
                if let Ok(mut slot) = text.lock() {
                    *slot = String::from_utf8_lossy(&buf).trim().to_owned();
                }
                let _ = io::copy(&mut se.take(1024 * 1024), &mut io::sink());
            })
        });
        let session = RootSession {
            pipes: Arc::new(Mutex::new(Some(pipes))),
            child: Arc::new(Mutex::new(Some(child))),
            stderr_text,
            stderr_thread: Arc::new(Mutex::new(stderr_thread)),
        };
        match session.call(Request::Ping).await {
            Ok(Reply::Ok) => Ok(session),
            Ok(Reply::Error(e)) => {
                let _ = session.failure_reason().await;
                Err(e)
            }
            Ok(Reply::Entries(_)) => {
                let _ = session.failure_reason().await;
                Err("Unexpected reply from root helper".into())
            }
            Err(_) => Err(session.failure_reason().await),
        }
    }

    async fn failure_reason(&self) -> String {
        let pipes = self.pipes.clone();
        let child = self.child.clone();
        let stderr_text = self.stderr_text.clone();
        let stderr_thread = self.stderr_thread.clone();
        gio::spawn_blocking(move || {
            if let Ok(mut p) = pipes.lock() {
                p.take();
            }
            let child = child.lock().ok().and_then(|mut c| c.take());
            let Some(mut child) = child else {
                return "Root mode is unavailable".into();
            };
            let status = child.wait();
            if let Some(handle) = stderr_thread.lock().ok().and_then(|mut t| t.take()) {
                let _ = handle.join();
            }
            match status {
                Ok(status) => match status.code() {
                    Some(126) => "Authentication was cancelled".into(),
                    Some(127) => "Not authorized".into(),
                    _ => {
                        let text = stderr_text
                            .lock()
                            .map(|s| s.clone())
                            .unwrap_or_default();
                        if text.is_empty() {
                            "Root mode failed to start".into()
                        } else {
                            text
                        }
                    }
                },
                Err(e) => e.to_string(),
            }
        })
        .await
        .unwrap_or_else(|_| "Root mode failed to start".into())
    }

    pub async fn call(&self, request: Request) -> Result<Reply, String> {
        let pipes = self.pipes.clone();
        gio::spawn_blocking(move || {
            let mut guard = pipes
                .lock()
                .map_err(|_| "helper lock poisoned".to_string())?;
            let pipes_ref = guard.as_mut().ok_or("root mode has ended")?;
            let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
            line.push('\n');
            if let Err(e) = pipes_ref
                .stdin
                .write_all(line.as_bytes())
                .and_then(|_| pipes_ref.stdin.flush())
            {
                guard.take();
                return Err(e.to_string());
            }
            let mut answer = String::new();
            match pipes_ref.stdout.read_line(&mut answer) {
                Ok(0) => {
                    guard.take();
                    return Err("the root helper exited".into());
                }
                Ok(_) => {}
                Err(e) => {
                    guard.take();
                    return Err(e.to_string());
                }
            }
            serde_json::from_str::<Reply>(&answer).map_err(|e| {
                guard.take();
                e.to_string()
            })
        })
        .await
        .map_err(|_| "root helper worker failed".to_string())?
    }

    pub async fn run(&self, request: Request) -> Result<(), String> {
        match self.call(request).await? {
            Reply::Ok => Ok(()),
            Reply::Error(e) => Err(e),
            Reply::Entries(_) => Err("unexpected reply".into()),
        }
    }

    pub async fn list(&self, path: &Path) -> Result<Vec<Entry>, String> {
        match self
            .call(Request::List {
                path: path.to_path_buf(),
            })
            .await?
        {
            Reply::Entries(entries) => Ok(entries),
            Reply::Error(e) => Err(e),
            Reply::Ok => Err("unexpected reply".into()),
        }
    }
}

impl Drop for RootSession {
    fn drop(&mut self) {
        let pipes = self.pipes.clone();
        let child = self.child.clone();
        let stderr_thread = self.stderr_thread.clone();
        std::thread::spawn(move || {
            if let Ok(mut p) = pipes.lock() {
                p.take();
            }
            if let Some(mut c) = child.lock().ok().and_then(|mut g| g.take()) {
                let _ = c.wait();
            }
            if let Some(handle) = stderr_thread.lock().ok().and_then(|mut t| t.take()) {
                let _ = handle.join();
            }
        });
    }
}

pub fn file_entries(dir: &Path, entries: &[Entry]) -> Vec<FileEntry> {
    let mut ctx = ScanContext::new();
    entries
        .iter()
        .map(|e| ctx.from_root_entry(dir, e))
        .collect()
}
