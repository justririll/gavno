//! Pure-Rust Freedesktop Trash specification implementation (no GVfs required).
use crate::model::{self, FileEntry, ScanContext, TrashMeta};
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
    },
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn trash_root() -> PathBuf {
    glib::user_data_dir().join("Trash")
}

fn ensure_dirs(root: &Path) -> Result<(PathBuf, PathBuf), String> {
    use std::os::unix::fs::DirBuilderExt;
    let files = root.join("files");
    let info = root.join("info");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&files)
        .map_err(|e| format!("Cannot create Trash files directory: {e}"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&info)
        .map_err(|e| format!("Cannot create Trash info directory: {e}"))?;
    Ok((files, info))
}

fn trashinfo_path(info_dir: &Path, trash_name: &OsStr) -> PathBuf {
    let mut name = trash_name.to_os_string();
    name.push(".trashinfo");
    info_dir.join(name)
}

pub fn encode_trash_path(path: &Path) -> String {
    let bytes = path.as_os_str().as_bytes();
    let mut out = String::with_capacity(bytes.len() + 8);
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

pub fn decode_trash_path(text: &str) -> PathBuf {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(OsString::from_vec(out))
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn parse_trashinfo(text: &str) -> Option<(PathBuf, String)> {
    let mut in_section = false;
    let mut orig_path = None;
    let mut deletion_date = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[Trash Info]";
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Path=") {
            orig_path = Some(decode_trash_path(rest));
        } else if let Some(rest) = line.strip_prefix("DeletionDate=") {
            deletion_date = rest.to_owned();
        }
    }
    orig_path.map(|p| (p, deletion_date))
}

fn format_iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    match model::local_tm(secs) {
        Some(tm) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        None => "1970-01-01T00:00:00".to_owned(),
    }
}

pub fn trash_count() -> usize {
    let dir = trash_root().join("files");
    fs::read_dir(dir).map(|rd| rd.count()).unwrap_or(0)
}

pub fn list_trash(cancel: &AtomicBool) -> Result<Vec<FileEntry>, String> {
    list_trash_in(&trash_root(), cancel)
}

pub fn list_trash_in(root: &Path, cancel: &AtomicBool) -> Result<Vec<FileEntry>, String> {
    let files_dir = root.join("files");
    let info_dir = root.join("info");
    let rd = match fs::read_dir(&files_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("Cannot read Trash: {e}")),
    };
    let mut ctx = ScanContext::new();
    let mut out = Vec::new();
    for (idx, item) in rd.enumerate() {
        if idx & 63 == 0 && cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".to_owned());
        }
        let Ok(item) = item else { continue };
        let trash_os = item.file_name();
        let trash_lossy = trash_os.to_string_lossy().into_owned();
        let file_path = item.path();
        let info_path = trashinfo_path(&info_dir, &trash_os);
        let meta = fs::read_to_string(&info_path)
            .ok()
            .and_then(|s| parse_trashinfo(&s));
        let Some(mut entry) = ctx.build_entry(file_path, trash_lossy.clone(), None, false) else {
            continue;
        };
        if let Some((orig_path, deletion_date)) = meta {
            let orig_name = orig_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| trash_lossy.clone());
            entry.display_name = orig_name.clone();
            entry.lower_name = orig_name.to_lowercase();
            if let Some(parent) = orig_path.parent() {
                entry.subtitle = Some(parent.to_string_lossy().into_owned());
            }
            entry.trash = Some(TrashMeta {
                trash_name: trash_os.clone(),
                orig_path,
                deletion_date,
            });
        } else {
            entry.trash = Some(TrashMeta {
                trash_name: trash_os.clone(),
                orig_path: PathBuf::from(&trash_os),
                deletion_date: String::new(),
            });
        }
        entry.uri = format!("trash:///{}", encode_trash_path(Path::new(&trash_os)));
        out.push(entry);
    }
    Ok(out)
}

pub fn move_to_trash(path: &Path) -> Result<TrashMeta, String> {
    move_to_trash_in(path, &trash_root())
}

pub fn move_to_trash_in(path: &Path, root: &Path) -> Result<TrashMeta, String> {
    let raw_bytes = path.as_os_str().as_bytes();
    let trimmed = {
        let mut end = raw_bytes.len();
        while end > 1 && raw_bytes[end - 1] == b'/' {
            end -= 1;
        }
        &raw_bytes[..end]
    };
    if trimmed.is_empty()
        || trimmed == b"/"
        || trimmed == b"."
        || trimmed == b".."
        || trimmed.ends_with(b"/.")
        || trimmed.ends_with(b"/..")
        || matches!(
            path.components().next_back(),
            Some(Component::CurDir | Component::ParentDir | Component::RootDir)
        )
    {
        return Err("Cannot trash this path".to_owned());
    }
    let trimmed_path = Path::new(OsStr::from_bytes(trimmed));
    let abs = if trimmed_path.is_absolute() {
        trimmed_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(trimmed_path)
    };
    let base_os = abs
        .file_name()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "Cannot trash root directory".to_owned())?
        .to_os_string();
    let base_lossy = base_os.to_string_lossy().into_owned();
    let (files_dir, info_dir) = ensure_dirs(root)?;
    let deletion_date = format_iso_now();
    let payload = format!(
        "[Trash Info]\nPath={}\nDeletionDate={deletion_date}\n",
        encode_trash_path(&abs)
    );

    // Reserve a unique name atomically via O_CREAT|O_EXCL on the .trashinfo file.
    let (stem, ext) = match base_lossy.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
        _ => (base_lossy, String::new()),
    };
    let mut chosen_name: Option<OsString> = None;
    let mut info_path = PathBuf::new();
    for attempt in 1..10_000u32 {
        let candidate: OsString = if attempt == 1 {
            base_os.clone()
        } else {
            OsString::from(format!("{stem}.{attempt}{ext}"))
        };
        let candidate_info = trashinfo_path(&info_dir, &candidate);
        let candidate_file = files_dir.join(&candidate);
        if fs::symlink_metadata(&candidate_file).is_ok() {
            continue;
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate_info)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(payload.as_bytes()) {
                    let _ = fs::remove_file(&candidate_info);
                    return Err(format!("Cannot write trashinfo: {e}"));
                }
                chosen_name = Some(candidate);
                info_path = candidate_info;
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Cannot create trashinfo: {e}")),
        }
    }
    let trash_name = chosen_name.ok_or_else(|| "Unable to allocate Trash slot".to_owned())?;
    let dest = files_dir.join(&trash_name);
    if let Err(err) = move_path(&abs, &dest) {
        if !err.keep_dest {
            let _ = fs::remove_file(&info_path);
        }
        return Err(err.message);
    }
    Ok(TrashMeta {
        trash_name,
        orig_path: abs,
        deletion_date,
    })
}

pub fn restore_item(meta: &TrashMeta, target: &Path) -> Result<(), String> {
    restore_item_in(meta, target, &trash_root())
}

pub fn restore_item_in(meta: &TrashMeta, target: &Path, root: &Path) -> Result<(), String> {
    let files_dir = root.join("files");
    let info_dir = root.join("info");
    let src = files_dir.join(&meta.trash_name);
    if fs::symlink_metadata(&src).is_err() {
        return Err(format!(
            "Trashed item “{}” no longer exists",
            meta.trash_name.to_string_lossy()
        ));
    }
    if fs::symlink_metadata(target).is_ok() {
        return Err(format!("“{}” already exists", target.display()));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Cannot create parent folder: {e}"))?;
    }
    move_path(&src, target).map_err(|e| e.message)?;
    let _ = fs::remove_file(trashinfo_path(&info_dir, &meta.trash_name));
    Ok(())
}

pub fn delete_trashed(trash_name: &OsStr) -> Result<(), String> {
    delete_trashed_in(trash_name, &trash_root())
}

pub fn delete_trashed_in(trash_name: &OsStr, root: &Path) -> Result<(), String> {
    let file_path = root.join("files").join(trash_name);
    let info_path = trashinfo_path(&root.join("info"), trash_name);
    if let Ok(meta) = fs::symlink_metadata(&file_path) {
        if !meta.file_type().is_symlink() && meta.is_dir() {
            fs::remove_dir_all(&file_path).map_err(|e| e.to_string())?;
        } else {
            fs::remove_file(&file_path).map_err(|e| e.to_string())?;
        }
    }
    match fs::remove_file(&info_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    Ok(())
}

pub fn empty_trash() -> Result<usize, String> {
    empty_trash_in(&trash_root())
}

pub fn empty_trash_in(root: &Path) -> Result<usize, String> {
    let files_dir = root.join("files");
    let info_dir = root.join("info");
    let mut removed = 0usize;
    if let Ok(rd) = fs::read_dir(&files_dir) {
        for item in rd.flatten() {
            delete_trashed_in(&item.file_name(), root)?;
            removed += 1;
        }
    }
    if let Ok(rd) = fs::read_dir(&info_dir) {
        for item in rd.flatten() {
            let _ = fs::remove_file(item.path());
        }
    }
    Ok(removed)
}

struct MoveError {
    message: String,
    keep_dest: bool,
}

fn move_path(from: &Path, to: &Path) -> Result<(), MoveError> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            if let Err(copy_err) = copy_recursive(from, to) {
                let _ = remove_recursive(to);
                return Err(MoveError {
                    message: copy_err.to_string(),
                    keep_dest: false,
                });
            }
            if let Err(rm_err) = remove_recursive(from) {
                return Err(MoveError {
                    message: rm_err.to_string(),
                    keep_dest: true,
                });
            }
            Ok(())
        }
        Err(e) => Err(MoveError {
            message: e.to_string(),
            keep_dest: false,
        }),
    }
}

fn is_cross_device(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EXDEV)
}

fn copy_recursive(from: &Path, to: &Path) -> io::Result<()> {
    let mut dir_perms = Vec::new();
    copy_recursive_inner(from, to, &mut dir_perms)?;
    for (dir, perms) in dir_perms.into_iter().rev() {
        let _ = fs::set_permissions(&dir, perms);
    }
    Ok(())
}

fn copy_recursive_inner(
    from: &Path,
    to: &Path,
    dir_perms: &mut Vec<(PathBuf, fs::Permissions)>,
) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        let link = fs::read_link(from)?;
        std::os::unix::fs::symlink(link, to)?;
    } else if meta.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive_inner(&entry.path(), &to.join(entry.file_name()), dir_perms)?;
        }
        dir_perms.push((to.to_path_buf(), meta.permissions()));
    } else if meta.file_type().is_file() {
        fs::copy(from, to)?;
        let _ = fs::set_permissions(to, meta.permissions());
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cannot copy special file (FIFO, socket, or device) into Trash",
        ));
    }
    Ok(())
}

fn remove_recursive(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_symlink() && meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gavno-trash-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn encode_decode_unicode_and_spaces() {
        let orig = Path::new("/home/user/Документы/file #1 (test)%.txt");
        let encoded = encode_trash_path(orig);
        assert_eq!(decode_trash_path(&encoded), orig);
    }

    #[test]
    fn trash_collision_list_restore_and_empty() {
        let root = temp_dir("flow");
        let trash = root.join("Trash");
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();

        let f1 = work.join("hello world.txt");
        fs::write(&f1, b"first").unwrap();
        let m1 = move_to_trash_in(&f1, &trash).unwrap();
        assert!(!f1.exists());
        assert_eq!(m1.trash_name, OsStr::new("hello world.txt"));

        // Second file with same name gets a unique trash name.
        fs::write(&f1, b"second").unwrap();
        let m2 = move_to_trash_in(&f1, &trash).unwrap();
        assert_ne!(m2.trash_name, m1.trash_name);

        let listed = list_trash_in(&trash, &AtomicBool::new(false)).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|e| e.display_name == "hello world.txt"));

        // Restore over an existing broken symlink must fail instead of overwriting.
        std::os::unix::fs::symlink(work.join("missing"), &f1).unwrap();
        assert!(restore_item_in(&m1, &f1, &trash).is_err());
        fs::remove_file(&f1).unwrap();

        // Restore first item back.
        restore_item_in(&m1, &f1, &trash).unwrap();
        assert_eq!(fs::read_to_string(&f1).unwrap(), "first");

        // Empty remaining item.
        assert_eq!(empty_trash_in(&trash).unwrap(), 1);
        assert!(list_trash_in(&trash, &AtomicBool::new(false))
            .unwrap()
            .is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_dot_and_handles_non_utf8_trash_entries() {
        let root = temp_dir("nonutf8");
        let trash = root.join("Trash");
        assert!(move_to_trash_in(Path::new("."), &trash).is_err());

        let bad_os = OsString::from_vec(b"bad-\xff-name.txt".to_vec());
        let (files_dir, info_dir) = ensure_dirs(&trash).unwrap();
        fs::write(files_dir.join(&bad_os), b"bytes").unwrap();
        let orig = PathBuf::from(OsString::from_vec(b"/tmp/orig-\xff.txt".to_vec()));
        fs::write(
            trashinfo_path(&info_dir, &bad_os),
            format!(
                "[Trash Info]\nPath={}\nDeletionDate=2026-11-23T12:00:00\n",
                encode_trash_path(&orig)
            ),
        )
        .unwrap();

        let listed = list_trash_in(&trash, &AtomicBool::new(false)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].trash.as_ref().unwrap().orig_path, orig);
        assert_eq!(empty_trash_in(&trash).unwrap(), 1);
        assert!(list_trash_in(&trash, &AtomicBool::new(false))
            .unwrap()
            .is_empty());

        let _ = fs::remove_dir_all(&root);
    }
}
