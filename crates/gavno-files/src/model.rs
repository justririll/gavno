use crate::{
    config::SortField,
    icons::{self, Icon},
    root_proto, thumbs,
};
use gio::prelude::*;
use std::{
    cmp::Ordering,
    collections::HashMap,
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Location {
    Local(PathBuf),
    Trash,
}

impl Location {
    pub fn from_gio_file(file: &gio::File) -> Result<Self, String> {
        match file.uri_scheme().as_deref() {
            Some("trash") => Ok(Self::Trash),
            Some("file") | None => file
                .path()
                .map(Self::Local)
                .ok_or_else(|| "Unsupported file location".to_owned()),
            Some(_) => Err("Network locations are not supported".to_owned()),
        }
    }

    pub fn from_uri(uri: &str) -> Result<Self, String> {
        if uri == "trash:" || uri.starts_with("trash://") {
            return Ok(Self::Trash);
        }
        Self::from_gio_file(&gio::File::for_uri(uri))
    }

    pub fn to_uri(&self) -> String {
        match self {
            Self::Local(path) => file_uri_from_path(path),
            Self::Trash => "trash:///".to_owned(),
        }
    }

    pub fn display_title(&self) -> String {
        match self {
            Self::Trash => "Trash".to_owned(),
            Self::Local(path) => {
                if path == Path::new("/") {
                    "System".to_owned()
                } else {
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned())
                }
            }
        }
    }

    pub fn parse_name(&self) -> String {
        match self {
            Self::Trash => "trash:///".to_owned(),
            Self::Local(path) => path.to_string_lossy().into_owned(),
        }
    }

    pub fn parent(&self) -> Option<Self> {
        match self {
            Self::Trash => None,
            Self::Local(path) => path.parent().map(|p| Self::Local(p.to_path_buf())),
        }
    }

    pub fn is_trash(&self) -> bool {
        matches!(self, Self::Trash)
    }

    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path.as_path()),
            Self::Trash => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashMeta {
    pub trash_name: std::ffi::OsString,
    pub orig_path: PathBuf,
    pub deletion_date: String,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub uri: String,
    pub name: String,
    pub display_name: String,
    pub lower_name: String,
    pub subtitle: Option<String>,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_broken_symlink: bool,
    pub symlink_target: Option<String>,
    pub is_hidden: bool,
    pub is_readonly: bool,
    pub size: u64,
    pub size_text: String,
    pub mtime: i64,
    pub mtime_text: String,
    pub mode: u32,
    pub owner_text: String,
    pub content_type: String,
    pub kind_text: String,
    pub icon: Icon,
    pub is_thumbnail_supported: bool,
    pub trash: Option<TrashMeta>,
}

#[derive(Clone, Debug)]
pub enum ScanError {
    PermissionDenied,
    NotFound(String),
    Other(String),
    Cancelled,
}

impl ScanError {
    pub fn from_io(err: &std::io::Error) -> Self {
        match err.kind() {
            std::io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            std::io::ErrorKind::NotFound => Self::NotFound(err.to_string()),
            _ => Self::Other(err.to_string()),
        }
    }
}

pub struct ScanResult {
    pub entries: Vec<FileEntry>,
    pub dir_writable: bool,
}

/// Per-scan lookup caches so uid/MIME/localtime lookups happen at most once per unique value.
pub struct ScanContext {
    home: PathBuf,
    euid: u32,
    egid: u32,
    groups: Vec<u32>,
    today: (i32, i32, i32),
    current_year: i32,
    owners: HashMap<u32, String>,
    descriptions: HashMap<String, String>,
}

impl ScanContext {
    pub fn new() -> Self {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as libc::time_t;
        let tm = local_tm(now_secs).unwrap_or_else(|| unsafe { std::mem::zeroed() });
        let euid = unsafe { libc::geteuid() };
        let egid = unsafe { libc::getegid() };
        let groups = current_groups();
        Self {
            home: glib::home_dir(),
            euid,
            egid,
            groups,
            today: (tm.tm_year, tm.tm_mon, tm.tm_mday),
            current_year: tm.tm_year,
            owners: HashMap::with_capacity(8),
            descriptions: HashMap::with_capacity(32),
        }
    }

    pub fn owner_name(&mut self, uid: u32) -> String {
        if let Some(name) = self.owners.get(&uid) {
            return name.clone();
        }
        let name = lookup_username(uid).unwrap_or_else(|| uid.to_string());
        self.owners.insert(uid, name.clone());
        name
    }

    pub fn type_description(&mut self, content_type: &str) -> String {
        if let Some(desc) = self.descriptions.get(content_type) {
            return desc.clone();
        }
        let desc = gio::content_type_get_description(content_type).to_string();
        self.descriptions.insert(content_type.to_owned(), desc.clone());
        desc
    }

    pub fn format_mtime(&self, mtime: i64) -> String {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        if mtime <= 0 {
            return "—".to_owned();
        }
        let Some(tm) = local_tm(mtime as libc::time_t) else {
            return "—".to_owned();
        };
        let mon = MONTHS.get(tm.tm_mon as usize).copied().unwrap_or("Jan");
        if (tm.tm_year, tm.tm_mon, tm.tm_mday) == self.today {
            format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
        } else if tm.tm_year == self.current_year {
            format!("{:02} {mon} {:02}:{:02}", tm.tm_mday, tm.tm_hour, tm.tm_min)
        } else {
            format!("{:02} {mon} {}", tm.tm_mday, tm.tm_year + 1900)
        }
    }

    pub fn is_readonly(&self, uid: u32, gid: u32, mode: u32, root_mode: bool) -> bool {
        if root_mode || self.euid == 0 {
            return false;
        }
        if uid == self.euid {
            return (mode & 0o200) == 0;
        }
        if gid == self.egid || self.groups.contains(&gid) {
            return (mode & 0o020) == 0;
        }
        (mode & 0o002) == 0
    }

    pub fn build_entry(
        &mut self,
        path: PathBuf,
        name: String,
        subtitle: Option<String>,
        root_mode: bool,
    ) -> Option<FileEntry> {
        let lmeta = fs::symlink_metadata(&path).ok()?;
        let is_symlink = lmeta.file_type().is_symlink();
        let (meta, is_broken_symlink, symlink_target) = if is_symlink {
            let target = fs::read_link(&path)
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            match fs::metadata(&path) {
                Ok(tmeta) => (tmeta, false, target),
                Err(_) => (lmeta.clone(), true, target),
            }
        } else {
            (lmeta, false, None)
        };
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() };
        let mtime = meta.mtime();
        let mode = meta.mode();
        let uid = meta.uid();
        let gid = meta.gid();
        let is_hidden = name.starts_with('.');
        let is_readonly = self.is_readonly(uid, gid, mode, root_mode);
        let owner_text = self.owner_name(uid);
        let mtime_text = self.format_mtime(mtime);
        let size_text = if is_dir {
            "—".to_owned()
        } else {
            format_size(size)
        };
        let (content_type, kind_text, icon) = if is_dir {
            let glyph = icons::folder_glyph(&path, &self.home);
            (
                "inode/directory".to_owned(),
                if is_symlink {
                    "Folder (link)".to_owned()
                } else {
                    "Folder".to_owned()
                },
                Icon::Folder(glyph),
            )
        } else if is_broken_symlink {
            (
                "inode/symlink".to_owned(),
                "Broken link".to_owned(),
                icons::file_icon(&name, false),
            )
        } else {
            let (ct, _) = gio::content_type_guess(Some(name.as_str()), None::<&[u8]>);
            let ct_str = ct.to_string();
            let is_text = is_textual_mime(&ct_str);
            let desc = self.type_description(&ct_str);
            let icon = icons::file_icon(&name, is_text);
            (ct_str, desc, icon)
        };
        let is_thumbnail_supported = !is_dir && !is_broken_symlink && thumbs::supported_name(&name);
        let uri = file_uri_from_path(&path);
        let lower_name = name.to_lowercase();
        Some(FileEntry {
            path,
            uri,
            display_name: name.clone(),
            name,
            lower_name,
            subtitle,
            is_dir,
            is_symlink,
            is_broken_symlink,
            symlink_target,
            is_hidden,
            is_readonly,
            size,
            size_text,
            mtime,
            mtime_text,
            mode,
            owner_text,
            content_type,
            kind_text,
            icon,
            is_thumbnail_supported,
            trash: None,
        })
    }

    pub fn from_root_entry(&mut self, dir: &Path, e: &root_proto::Entry) -> FileEntry {
        let path = dir.join(&e.name);
        let uri = file_uri_from_path(&path);
        let size = if e.dir { 0 } else { e.size };
        let size_text = if e.dir {
            "—".to_owned()
        } else {
            format_size(size)
        };
        let mtime_text = self.format_mtime(e.mtime);
        let owner_text = self.owner_name(e.uid);
        let (content_type, kind_text, icon) = if e.dir {
            let glyph = icons::folder_glyph(&path, &self.home);
            (
                "inode/directory".to_owned(),
                "Folder".to_owned(),
                Icon::Folder(glyph),
            )
        } else {
            let (ct, _) = gio::content_type_guess(Some(e.name.as_str()), None::<&[u8]>);
            let ct_str = ct.to_string();
            let desc = self.type_description(&ct_str);
            let icon = icons::file_icon(&e.name, is_textual_mime(&ct_str));
            (ct_str, desc, icon)
        };
        FileEntry {
            path,
            uri,
            display_name: e.name.clone(),
            name: e.name.clone(),
            lower_name: e.name.to_lowercase(),
            subtitle: None,
            is_dir: e.dir,
            is_symlink: e.symlink,
            is_broken_symlink: false,
            symlink_target: None,
            is_hidden: e.name.starts_with('.'),
            is_readonly: false,
            size,
            size_text,
            mtime: e.mtime,
            mtime_text,
            mode: e.mode,
            owner_text,
            content_type,
            kind_text,
            icon,
            is_thumbnail_supported: !e.dir && thumbs::supported_name(&e.name),
            trash: None,
        }
    }
}

pub fn is_textual_mime(ct: &str) -> bool {
    gio::content_type_is_a(ct, "text/plain")
        || ct == "application/json"
        || ct == "application/xml"
        || ct == "application/javascript"
        || ct == "application/x-shellscript"
        || ct == "application/toml"
}

pub fn path_is_writable(path: &Path) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

pub fn scan_directory(
    dir: &Path,
    root_mode: bool,
    cancel: &AtomicBool,
) -> Result<ScanResult, ScanError> {
    let read_dir = fs::read_dir(dir).map_err(|e| ScanError::from_io(&e))?;
    let mut ctx = ScanContext::new();
    let mut entries = Vec::new();
    let mut count = 0usize;
    for item in read_dir {
        if count & 63 == 0 && cancel.load(AtomicOrdering::Relaxed) {
            return Err(ScanError::Cancelled);
        }
        count += 1;
        let Ok(dir_entry) = item else { continue };
        let name = dir_entry.file_name().to_string_lossy().into_owned();
        if let Some(entry) = ctx.build_entry(dir_entry.path(), name, None, root_mode) {
            entries.push(entry);
        }
    }
    if cancel.load(AtomicOrdering::Relaxed) {
        return Err(ScanError::Cancelled);
    }
    let dir_writable = root_mode || path_is_writable(dir);
    Ok(ScanResult {
        entries,
        dir_writable,
    })
}

pub fn entry_from_path(path: &Path, root_mode: bool) -> Option<FileEntry> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let mut ctx = ScanContext::new();
    ctx.build_entry(path.to_path_buf(), name, None, root_mode)
}

pub fn sort_entries(entries: &mut [FileEntry], field: SortField, descending: bool) {
    entries.sort_by(|a, b| {
        let dir_ord = b.is_dir.cmp(&a.is_dir);
        if dir_ord != Ordering::Equal {
            return dir_ord;
        }
        let primary = match field {
            SortField::Name => natural_cmp_lower(&a.lower_name, &b.lower_name),
            SortField::Size => a.size.cmp(&b.size),
            SortField::Modified => a.mtime.cmp(&b.mtime),
            SortField::Type => natural_cmp(&a.kind_text, &b.kind_text),
        };
        let primary = if descending {
            primary.reverse()
        } else {
            primary
        };
        if primary != Ordering::Equal {
            return primary;
        }
        natural_cmp_lower(&a.lower_name, &b.lower_name)
    });
}

pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    natural_cmp_lower(&a.to_lowercase(), &b.to_lowercase())
}

pub fn natural_cmp_lower(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (start_a, start_b) = (i, j);
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let mut aa = &a[start_a..i];
            let mut bb = &b[start_b..j];
            while aa.first() == Some(&b'0') && aa.len() > 1 {
                aa = &aa[1..];
            }
            while bb.first() == Some(&b'0') && bb.len() > 1 {
                bb = &bb[1..];
            }
            let cmp = aa.len().cmp(&bb.len()).then_with(|| aa.cmp(bb));
            if cmp != Ordering::Equal {
                return cmp;
            }
        } else {
            let cmp = a[i].cmp(&b[j]);
            if cmp != Ordering::Equal {
                return cmp;
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j)).then_with(|| a.cmp(b))
}

pub fn file_uri_from_path(path: &Path) -> String {
    let bytes = path.as_os_str().as_bytes();
    let mut out = String::with_capacity(7 + bytes.len() + 8);
    out.push_str("file://");
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

pub fn permissions_text(is_dir: bool, mode: u32) -> String {
    let mut s = String::from(if is_dir { "d" } else { "-" });
    for (bit, c) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ] {
        s.push(if mode & bit != 0 { c } else { '-' });
    }
    for (bit, index, yes, no) in [
        (0o4000, 3, 's', 'S'),
        (0o2000, 6, 's', 'S'),
        (0o1000, 9, 't', 'T'),
    ] {
        if mode & bit != 0 {
            let c = if s.as_bytes()[index] == b'x' { yes } else { no };
            s.replace_range(index..index + 1, &c.to_string());
        }
    }
    format!("{s} ({:04o})", mode & 0o7777)
}

/// Disk capacity the way drives are labelled: decimal units, whole numbers.
pub fn format_capacity(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000. && unit < units.len() - 1 {
        value /= 1000.;
        unit += 1;
    }
    if value < 10. && unit > 0 {
        format!("{value:.1} {}", units[unit])
    } else {
        format!("{value:.0} {}", units[unit])
    }
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit < units.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if value < 10. {
        format!("{value:.1} {}", units[unit])
    } else {
        format!("{value:.0} {}", units[unit])
    }
}

pub fn breadcrumbs(path: &Path, home: &Path) -> Vec<(String, PathBuf)> {
    let (mut result, mut acc, tail) = if let Ok(tail) = path.strip_prefix(home) {
        (vec![("Home".into(), home.into())], home.to_path_buf(), tail)
    } else {
        (
            vec![("/".into(), PathBuf::from("/"))],
            PathBuf::from("/"),
            path,
        )
    };
    for part in tail.components() {
        if let Component::Normal(name) = part {
            acc.push(name);
            result.push((name.to_string_lossy().into_owned(), acc.clone()));
        }
    }
    result
}

pub fn local_tm(ts: libc::time_t) -> Option<libc::tm> {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ptr = unsafe { libc::localtime_r(&ts, &mut tm) };
    if ptr.is_null() {
        None
    } else {
        Some(tm)
    }
}

pub fn lookup_username(uid: u32) -> Option<String> {
    let mut buffer = vec![0u8; 4096];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

fn current_groups() -> Vec<u32> {
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if n <= 0 {
        return Vec::new();
    }
    let mut groups = vec![0 as libc::gid_t; n as usize];
    let got = unsafe { libc::getgroups(n, groups.as_mut_ptr()) };
    if got > 0 {
        groups.truncate(got as usize);
        groups
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_unicode() {
        let mut names = vec![
            "file10",
            "file2",
            "file1",
            "File20",
            "file99999999999999999999999",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            [
                "file1",
                "file2",
                "file10",
                "File20",
                "file99999999999999999999999"
            ]
        );
        assert_eq!(natural_cmp("α2", "α10"), Ordering::Less);
        assert_eq!(natural_cmp("file2", "file02"), Ordering::Greater);
        assert_eq!(natural_cmp("", "a"), Ordering::Less);
    }

    #[test]
    fn path_segments() {
        let home = Path::new("/home/test");
        assert_eq!(
            breadcrumbs(Path::new("/"), home),
            vec![("/".into(), PathBuf::from("/"))]
        );
        assert_eq!(
            breadcrumbs(Path::new("/home/test/a b/文"), home),
            vec![
                ("Home".into(), home.into()),
                ("a b".into(), home.join("a b")),
                ("文".into(), home.join("a b/文"))
            ]
        );
        assert_eq!(breadcrumbs(Path::new("/home/test2"), home)[0].0, "/");
        assert_eq!(breadcrumbs(home, home).len(), 1);
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(10 * 1024), "10 KiB");
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
        assert_eq!(format_size(u64::MAX), "16 EiB");
    }

    #[test]
    fn uri_encoding_matches_gio() {
        assert_eq!(
            file_uri_from_path(Path::new("/tmp/a b/файл#1.txt")),
            "file:///tmp/a%20b/%D1%84%D0%B0%D0%B9%D0%BB%231.txt"
        );
    }

    #[test]
    fn scan_and_sort_folders_always_first() {
        let dir = std::env::temp_dir().join(format!("gavno-model-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("zfolder")).unwrap();
        fs::create_dir_all(dir.join("afolder")).unwrap();
        fs::write(dir.join("file2.txt"), b"22").unwrap();
        fs::write(dir.join("file10.txt"), b"123456").unwrap();

        let cancel = AtomicBool::new(false);
        let mut res = scan_directory(&dir, false, &cancel).unwrap();
        assert!(res.dir_writable);
        sort_entries(&mut res.entries, SortField::Name, false);
        let names: Vec<_> = res.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["afolder", "zfolder", "file2.txt", "file10.txt"]);

        sort_entries(&mut res.entries, SortField::Name, true);
        let names_desc: Vec<_> = res.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names_desc, ["zfolder", "afolder", "file10.txt", "file2.txt"]);

        sort_entries(&mut res.entries, SortField::Size, true);
        let names_size: Vec<_> = res.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names_size, ["afolder", "zfolder", "file10.txt", "file2.txt"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_and_sort_two_thousand_files() {
        let dir = std::env::temp_dir().join(format!("gavno-perf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("00_folder")).unwrap();
        for i in 0..2000 {
            fs::write(dir.join(format!("item_{i}.txt")), b"x").unwrap();
        }
        let started = std::time::Instant::now();
        let cancel = AtomicBool::new(false);
        let mut res = scan_directory(&dir, false, &cancel).unwrap();
        sort_entries(&mut res.entries, SortField::Name, false);
        let elapsed = started.elapsed();

        assert_eq!(res.entries.len(), 2001);
        assert!(res.entries[0].is_dir);
        assert_eq!(res.entries[1].name, "item_0.txt");
        assert_eq!(res.entries[2].name, "item_1.txt");
        assert_eq!(res.entries[10].name, "item_9.txt");
        assert_eq!(res.entries[11].name, "item_10.txt");
        assert!(elapsed < std::time::Duration::from_secs(3), "scan+sort took {elapsed:?}");

        let _ = fs::remove_dir_all(&dir);
    }
}
