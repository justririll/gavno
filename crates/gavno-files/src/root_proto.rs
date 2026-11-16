//! Protocol of gavno-files-helper, the small program that root mode starts
//! through pkexec. One JSON request per line on stdin, one JSON reply per
//! line on stdout. The helper only ever acts on absolute paths it is given;
//! the UI itself never runs as root.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, BufRead, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    List { path: PathBuf },
    Mkdir { path: PathBuf },
    Touch { path: PathBuf },
    Rename { from: PathBuf, to: PathBuf },
    Copy { from: PathBuf, to: PathBuf },
    Move { from: PathBuf, to: PathBuf },
    Delete { path: PathBuf },
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
    if path.is_absolute() && !path.as_os_str().is_empty() {
        Ok(path)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "path must be absolute"))
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn refuse_existing(to: &Path) -> io::Result<()> {
    if exists(to) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        ));
    }
    Ok(())
}

fn list(path: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(path)? {
        let item = item?;
        let Ok(meta) = fs::symlink_metadata(item.path()) else { continue };
        let symlink = meta.file_type().is_symlink();
        // Follow links for the type and size, as file managers show them.
        let target = if symlink { fs::metadata(item.path()).unwrap_or(meta.clone()) } else { meta.clone() };
        entries.push(Entry {
            name: item.file_name().to_string_lossy().into_owned(),
            dir: target.is_dir(),
            symlink,
            size: target.len(),
            mtime: meta.mtime(),
            mode: meta.permissions().mode(),
            uid: meta.uid(),
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
        if to.starts_with(from) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cannot copy a folder into itself"));
        }
        fs::create_dir(to)?;
        for item in fs::read_dir(from)? {
            let item = item?;
            copy(&item.path(), &to.join(item.file_name()))?;
        }
        fs::set_permissions(to, meta.permissions())?;
    } else {
        fs::copy(from, to)?;
    }
    Ok(())
}

fn delete(path: &Path) -> io::Result<()> {
    if path == Path::new("/") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to delete /"));
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
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
                fs::OpenOptions::new().write(true).create_new(true).open(absolute(path)?)?;
                Reply::Ok
            }
            Request::Rename { from, to } => {
                refuse_existing(absolute(to)?)?;
                fs::rename(absolute(from)?, to)?;
                Reply::Ok
            }
            Request::Copy { from, to } => {
                refuse_existing(absolute(to)?)?;
                copy(absolute(from)?, to)?;
                Reply::Ok
            }
            Request::Move { from, to } => {
                refuse_existing(absolute(to)?)?;
                let from = absolute(from)?;
                if fs::rename(from, to).is_err() {
                    // Across filesystems: copy, then remove the source.
                    copy(from, to)?;
                    delete(from)?;
                }
                Reply::Ok
            }
            Request::Delete { path } => {
                delete(absolute(path)?)?;
                Reply::Ok
            }
        })
    })();
    result.unwrap_or_else(|e| Reply::Error(e.to_string()))
}

/// The helper's main loop; returns when stdin closes.
pub fn serve(input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(request) => handle(&request),
            Err(e) => Reply::Error(format!("bad request: {e}")),
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
        ok(Request::Mkdir { path: root.join("a") });
        ok(Request::Touch { path: root.join("a/f") });
        fs::write(root.join("a/f"), b"hello").unwrap();
        ok(Request::Copy { from: root.join("a"), to: root.join("b") });
        assert_eq!(fs::read(root.join("b/f")).unwrap(), b"hello");
        ok(Request::Rename { from: root.join("b"), to: root.join("c") });
        ok(Request::Move { from: root.join("c/f"), to: root.join("g") });
        let Reply::Entries(mut entries) = handle(&Request::List { path: root.clone() }) else {
            panic!("list failed");
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.dir)).collect();
        assert_eq!(names, [("a", true), ("c", true), ("g", false)]);
        assert_eq!(entries[2].size, 5);
        ok(Request::Delete { path: root.join("a") });
        assert!(!root.join("a").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refuses_relative_paths_overwrites_and_self_copies() {
        let root = temp();
        assert!(matches!(handle(&Request::Mkdir { path: "rel".into() }), Reply::Error(_)));
        ok(Request::Mkdir { path: root.join("a") });
        ok(Request::Mkdir { path: root.join("b") });
        assert!(matches!(
            handle(&Request::Rename { from: root.join("a"), to: root.join("b") }),
            Reply::Error(_)
        ));
        assert!(matches!(
            handle(&Request::Copy { from: root.join("a"), to: root.join("a/inner") }),
            Reply::Error(_)
        ));
        assert!(matches!(handle(&Request::Delete { path: "/".into() }), Reply::Error(_)));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn serve_answers_each_line() {
        let input = b"{\"op\":\"ping\"}\nnot json\n";
        let mut output = Vec::new();
        serve(&input[..], &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "\"ok\"");
        assert!(lines[1].contains("bad request"));
    }
}
