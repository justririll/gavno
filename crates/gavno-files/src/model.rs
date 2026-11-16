use gio::prelude::*;
use std::{
    cmp::Ordering,
    path::{Component, Path, PathBuf},
};

pub const ATTRIBUTES: &str =
    "standard::*,time::modified,unix::mode,trash::orig-path,trash::deletion-date,id::file";
pub const BATCH: i32 = 128;

// FileInfo is already a GObject, so the model needs no application subclass.
pub fn attach(info: &gio::FileInfo, file: &gio::File) {
    info.set_attribute_string("gavno::uri", file.uri().as_str());
}
pub fn file(info: &gio::FileInfo) -> gio::File {
    gio::File::for_uri(
        info.attribute_string("gavno::uri")
            .as_deref()
            .unwrap_or("file:///"),
    )
}
pub fn is_dir(info: &gio::FileInfo) -> bool {
    info.file_type() == gio::FileType::Directory
}
pub fn kind(info: &gio::FileInfo) -> String {
    if is_dir(info) {
        return "Folder".into();
    }
    info.content_type()
        .map(|t| gio::content_type_get_description(&t).to_string())
        .unwrap_or_else(|| "File".into())
}
pub fn modified(info: &gio::FileInfo) -> String {
    info.modification_date_time()
        .and_then(|d| d.format("%d %b %Y %H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "—".into())
}
pub fn permissions(info: &gio::FileInfo) -> String {
    if !info.has_attribute("unix::mode") {
        return "Unavailable".into();
    }
    let mode = info.attribute_uint32("unix::mode");
    let mut s = String::from(if is_dir(info) { "d" } else { "-" });
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

pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
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
}

// FileMonitor is thread-affine. Its GLib loop and all filesystem setup live here;
// GTK receives only lightweight change notifications.
pub fn watch_directory(
    file: gio::File,
    cancel: gio::Cancellable,
) -> std::sync::mpsc::Receiver<Result<(), String>> {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let context = glib::MainContext::new();
        let error_send = send.clone();
        let result = context.with_thread_default(|| {
            let main_loop = glib::MainLoop::new(Some(&context), false);
            let quit_loop = main_loop.clone();
            let handler = cancel.connect_cancelled(move |_| quit_loop.quit());
            // Also observe cancellation in the owned context, covering a cancel
            // arriving between the final check and MainLoop::run.
            let stop_loop = main_loop.clone();
            let stop_cancel = cancel.clone();
            let stop = glib::timeout_source_new(
                std::time::Duration::from_millis(100),
                None,
                glib::Priority::LOW,
                move || {
                    if stop_cancel.is_cancelled() {
                        stop_loop.quit();
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                },
            );
            stop.attach(Some(&context));
            if cancel.is_cancelled() {
                if let Some(handler) = handler {
                    cancel.disconnect_cancelled(handler);
                }
                stop.destroy();
                return;
            }
            match file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, Some(&cancel)) {
                Ok(monitor) => {
                    monitor.connect_changed(move |_, _, _, _| {
                        let _ = send.send(Ok(()));
                    });
                    if !cancel.is_cancelled() {
                        main_loop.run();
                    }
                    monitor.cancel();
                }
                Err(e) => {
                    if !e.matches(gio::IOErrorEnum::Cancelled) {
                        let _ = send.send(Err(e.to_string()));
                    }
                }
            }
            stop.destroy();
            if let Some(handler) = handler {
                cancel.disconnect_cancelled(handler);
            }
        });
        if let Err(e) = result {
            let _ = error_send.send(Err(format!("File monitor context: {e}")));
        }
    });
    receive
}
