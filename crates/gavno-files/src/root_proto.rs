//! Protocol of gavno-files-helper, the small program that root mode starts
//! through pkexec. One JSON request per line on stdin, one JSON reply per
//! line on stdout. The helper only ever acts on absolute paths it is given;
//! the UI itself never runs as root.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, BufRead, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_BACKUP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    List {
        path: PathBuf,
    },
    Mkdir {
        path: PathBuf,
    },
    Touch {
        path: PathBuf,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[serde(default)]
        overwrite: bool,
    },
    Copy {
        from: PathBuf,
        to: PathBuf,
        #[serde(default)]
        overwrite: bool,
    },
    Move {
        from: PathBuf,
        to: PathBuf,
        #[serde(default)]
        overwrite: bool,
    },
    Delete {
        path: PathBuf,
    },
    Chmod {
        path: PathBuf,
        mode: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub symlink: bool,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    pub uid: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Ok,
    Entries(Vec<Entry>),
    Error(String),
}

fn absolute(path: &Path) -> io::Result<&Path> {
    if !path.is_absolute()
        || path.as_os_str().is_empty()
        || path.components().any(|c| matches!(c, Component::ParentDir))
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be normalized absolute",
        ))
    } else {
        Ok(path)
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        if !matches!(c, Component::CurDir) {
            out.push(c.as_os_str());
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

fn canonical_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let clean = normalize_lexical(path);
    let mut cur = clean.as_path();
    let mut tail = Vec::new();
    loop {
        if let Ok(canon) = fs::canonicalize(cur) {
            let mut res = canon;
            for part in tail.into_iter().rev() {
                res.push(part);
            }
            return Some(res);
        }
        let name = cur.file_name()?;
        tail.push(name.to_os_string());
        cur = cur.parent()?;
    }
}

fn canonical_entry_path(path: &Path) -> Option<PathBuf> {
    let clean = normalize_lexical(path);
    if clean == Path::new("/") {
        return Some(clean);
    }
    let name = clean.file_name()?;
    let parent = clean.parent().unwrap_or(Path::new("/"));
    Some(canonical_with_missing_tail(parent)?.join(name))
}

fn same_entry(a: &Path, b: &Path) -> bool {
    if normalize_lexical(a) == normalize_lexical(b) {
        return true;
    }
    if let (Some(ca), Some(cb)) = (canonical_entry_path(a), canonical_entry_path(b)) {
        if ca == cb {
            return true;
        }
    }
    false
}

fn is_descendant_of_real_dir(candidate: &Path, ancestor_dir: &Path) -> bool {
    let Ok(anc_meta) = fs::symlink_metadata(ancestor_dir) else {
        return false;
    };
    if anc_meta.file_type().is_symlink() || !anc_meta.is_dir() {
        return false;
    }
    let anc_id = (anc_meta.dev(), anc_meta.ino());
    let clean = normalize_lexical(candidate);
    let mut cur = Some(clean.as_path());
    while let Some(p) = cur {
        if let Ok(m) = fs::symlink_metadata(p) {
            if !m.file_type().is_symlink() && (m.dev(), m.ino()) == anc_id {
                return true;
            }
        }
        cur = p.parent();
    }
    let Ok(anc_canon) = fs::canonicalize(ancestor_dir) else {
        return false;
    };
    let Some(cand_canon) = canonical_entry_path(candidate) else {
        return false;
    };
    cand_canon == anc_canon || cand_canon.starts_with(&anc_canon)
}

fn is_path_prefix_entry(source: &Path, target: &Path) -> bool {
    let clean = normalize_lexical(source);
    let mut cur = clean.parent();
    while let Some(anc) = cur {
        if anc.as_os_str().is_empty() {
            break;
        }
        if same_entry(anc, target) {
            return true;
        }
        cur = anc.parent();
    }
    false
}

struct BackupGuard {
    target: PathBuf,
    backup: PathBuf,
    committed: bool,
}

impl BackupGuard {
    fn commit(mut self) {
        self.committed = true;
        let _ = delete(&self.backup);
    }
}

impl Drop for BackupGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = delete(&self.target);
            let _ = fs::rename(&self.backup, &self.target);
        }
    }
}

fn stage_destination(from: &Path, to: &Path, overwrite: bool) -> io::Result<Option<BackupGuard>> {
    if same_entry(from, to) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source and destination refer to the same entry",
        ));
    }
    if is_descendant_of_real_dir(from, to) || is_path_prefix_entry(from, to) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot overwrite an ancestor folder with its descendant",
        ));
    }
    if !exists(to) {
        return Ok(None);
    }
    if !overwrite {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        ));
    }
    let parent = to.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "cannot overwrite root directory")
    })?;
    let seq = NEXT_BACKUP_SEQ.fetch_add(1, Ordering::Relaxed);
    let backup = parent.join(format!(".gavno-root-bak-{}-{}", std::process::id(), seq));
    fs::rename(to, &backup)?;
    Ok(Some(BackupGuard {
        target: to.to_path_buf(),
        backup,
        committed: false,
    }))
}

fn list(path: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(path)? {
        let item = item?;
        let Ok(meta) = fs::symlink_metadata(item.path()) else {
            continue;
        };
        let symlink = meta.file_type().is_symlink();
        let target = if symlink {
            fs::metadata(item.path()).unwrap_or_else(|_| meta.clone())
        } else {
            meta.clone()
        };
        entries.push(Entry {
            name: item.file_name().to_string_lossy().into_owned(),
            dir: target.is_dir(),
            symlink,
            size: target.len(),
            mtime: target.mtime(),
            mode: target.permissions().mode(),
            uid: target.uid(),
        });
    }
    Ok(entries)
}

/// Recursive copy that keeps permission bits and copies links as links.
fn copy(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
    } else if meta.is_dir() {
        if same_entry(from, to) || is_descendant_of_real_dir(to, from) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot copy a folder into itself",
            ));
        }
        fs::create_dir(to)?;
        if let Err(e) = (|| -> io::Result<()> {
            for item in fs::read_dir(from)? {
                let item = item?;
                copy(&item.path(), &to.join(item.file_name()))?;
            }
            fs::set_permissions(to, meta.permissions())
        })() {
            let _ = fs::remove_dir_all(to);
            return Err(e);
        }
    } else if meta.file_type().is_file() {
        if let Err(e) = fs::copy(from, to).and_then(|_| fs::set_permissions(to, meta.permissions()))
        {
            let _ = fs::remove_file(to);
            return Err(e);
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot copy special file (FIFO, socket, or device)",
        ));
    }
    Ok(())
}

fn delete(path: &Path) -> io::Result<()> {
    let clean = normalize_lexical(path);
    if clean == Path::new("/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to delete /",
        ));
    }
    let meta = fs::symlink_metadata(&clean)?;
    if let Ok(root_meta) = fs::symlink_metadata("/") {
        if meta.dev() == root_meta.dev() && meta.ino() == root_meta.ino() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to delete /",
            ));
        }
    }
    if !meta.file_type().is_symlink() && meta.is_dir() {
        fs::remove_dir_all(&clean)
    } else {
        fs::remove_file(&clean)
    }
}

pub fn handle(request: &Request) -> Reply {
    let result = (|| -> io::Result<Reply> {
        Ok(match request {
            Request::Ping => Reply::Ok,
            Request::List { path } => Reply::Entries(list(absolute(path)?)?),
            Request::Mkdir { path } => {
                fs::create_dir(absolute(path)?)?;
                Reply::Ok
            }
            Request::Touch { path } => {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(absolute(path)?)?;
                Reply::Ok
            }
            Request::Rename {
                from,
                to,
                overwrite,
            } => {
                let from = absolute(from)?;
                let to = absolute(to)?;
                let _ = fs::symlink_metadata(from)?;
                if same_entry(from, to) {
                    return Ok(Reply::Ok);
                }
                let guard = stage_destination(from, to, *overwrite)?;
                fs::rename(from, to)?;
                if let Some(g) = guard {
                    g.commit();
                }
                Reply::Ok
            }
            Request::Copy {
                from,
                to,
                overwrite,
            } => {
                let from = absolute(from)?;
                let to = absolute(to)?;
                let _ = fs::symlink_metadata(from)?;
                let guard = stage_destination(from, to, *overwrite)?;
                copy(from, to)?;
                if let Some(g) = guard {
                    g.commit();
                }
                Reply::Ok
            }
            Request::Move {
                from,
                to,
                overwrite,
            } => {
                let from = absolute(from)?;
                let to = absolute(to)?;
                let _ = fs::symlink_metadata(from)?;
                if same_entry(from, to) {
                    return Ok(Reply::Ok);
                }
                let mut guard = stage_destination(from, to, *overwrite)?;
                match fs::rename(from, to) {
                    Ok(()) => {
                        if let Some(g) = guard.take() {
                            g.commit();
                        }
                    }
                    Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                        copy(from, to)?;
                        if let Some(g) = guard.take() {
                            g.commit();
                        }
                        delete(from)?;
                    }
                    Err(e) => return Err(e),
                }
                Reply::Ok
            }
            Request::Delete { path } => {
                delete(absolute(path)?)?;
                Reply::Ok
            }
            Request::Chmod { path, mode } => {
                fs::set_permissions(absolute(path)?, fs::Permissions::from_mode(*mode))?;
                Reply::Ok
            }
        })
    })();
    result.unwrap_or_else(|e| Reply::Error(e.to_string()))
}

/// The helper's main loop; returns when stdin closes.
pub fn serve(mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if input.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        let reply = match std::str::from_utf8(&buf) {
            Ok(line) if line.trim().is_empty() => continue,
            Ok(line) => match serde_json::from_str::<Request>(line) {
                Ok(request) => handle(&request),
                Err(e) => Reply::Error(format!("bad request: {e}")),
            },
            Err(e) => Reply::Error(format!("bad utf-8 request: {e}")),
        };
        serde_json::to_writer(&mut output, &reply)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gavno-root-proto-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ok(request: Request) {
        assert_eq!(handle(&request), Reply::Ok, "{request:?}");
    }

    #[test]
    fn operations_round_trip() {
        let root = temp();
        ok(Request::Mkdir {
            path: root.join("a"),
        });
        ok(Request::Touch {
            path: root.join("a/f"),
        });
        fs::write(root.join("a/f"), b"hello").unwrap();
        ok(Request::Copy {
            from: root.join("a"),
            to: root.join("b"),
            overwrite: false,
        });
        assert_eq!(fs::read(root.join("b/f")).unwrap(), b"hello");
        ok(Request::Rename {
            from: root.join("b"),
            to: root.join("c"),
            overwrite: false,
        });
        ok(Request::Move {
            from: root.join("c/f"),
            to: root.join("g"),
            overwrite: false,
        });
        ok(Request::Chmod {
            path: root.join("g"),
            mode: 0o640,
        });
        let Reply::Entries(mut entries) = handle(&Request::List { path: root.clone() }) else {
            panic!("list failed");
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.dir)).collect();
        assert_eq!(names, [("a", true), ("c", true), ("g", false)]);
        assert_eq!(entries[2].size, 5);
        assert_eq!(entries[2].mode & 0o777, 0o640);
        ok(Request::Delete {
            path: root.join("a"),
        });
        assert!(!root.join("a").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refuses_relative_paths_overwrites_and_self_copies() {
        let root = temp();
        assert!(matches!(
            handle(&Request::Mkdir { path: "rel".into() }),
            Reply::Error(_)
        ));
        ok(Request::Mkdir {
            path: root.join("a"),
        });
        ok(Request::Mkdir {
            path: root.join("b"),
        });
        assert!(matches!(
            handle(&Request::Rename {
                from: root.join("a"),
                to: root.join("b"),
                overwrite: false,
            }),
            Reply::Error(_)
        ));
        assert!(matches!(
            handle(&Request::Copy {
                from: root.join("a"),
                to: root.join("a/inner"),
                overwrite: false,
            }),
            Reply::Error(_)
        ));
        assert!(matches!(
            handle(&Request::Delete { path: "/".into() }),
            Reply::Error(_)
        ));
        assert!(matches!(
            handle(&Request::Delete {
                path: "/tmp/..".into()
            }),
            Reply::Error(_)
        ));

        let real = root.join("real");
        let link = root.join("link");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        fs::write(real.join("keep.txt"), b"keep").unwrap();
        assert!(matches!(
            handle(&Request::Copy {
                from: real.join("keep.txt"),
                to: link.join("keep.txt"),
                overwrite: true,
            }),
            Reply::Error(_)
        ));
        assert_eq!(fs::read(real.join("keep.txt")).unwrap(), b"keep");

        let link_file = root.join("sym");
        std::os::unix::fs::symlink(real.join("keep.txt"), &link_file).unwrap();
        fs::set_permissions(real.join("keep.txt"), fs::Permissions::from_mode(0o644)).unwrap();
        let Reply::Entries(sym_entries) = handle(&Request::List { path: root.clone() }) else {
            panic!("list failed");
        };
        let sym = sym_entries.iter().find(|e| e.name == "sym").unwrap();
        assert_eq!(sym.mode & 0o777, 0o644);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn serve_answers_each_line() {
        let mut input: Vec<u8> = b"{\"op\":\"ping\"}\n".to_vec();
        input.extend_from_slice(b"\xff\xfe\n");
        input.extend_from_slice(b"not json\n");
        let mut output = Vec::new();
        serve(&input[..], &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "\"ok\"");
        assert!(lines[1].contains("bad utf-8"));
        assert!(lines[2].contains("bad request"));
    }
}
