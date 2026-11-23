//! Fast pure-Rust BFS recursive search with immediate cancellation and batched UI streaming.
use crate::model::{FileEntry, ScanContext};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

pub const LIMIT: usize = 10_000;

/// Starts recursive search on a worker thread and streams matching batches to GTK without polling.
pub fn spawn(
    root: PathBuf,
    query: String,
    show_hidden: bool,
    cancel: Arc<AtomicBool>,
    on_batch: impl Fn(Vec<FileEntry>) + 'static,
    on_done: impl Fn(Result<bool, String>) + 'static,
) {
    let (batch_tx, batch_rx) = std::sync::mpsc::channel::<Vec<FileEntry>>();
    let waiter: Arc<Mutex<Option<futures_channel::oneshot::Sender<()>>>> =
        Arc::new(Mutex::new(None));
    let done_flag = Arc::new(Mutex::new(None::<Result<bool, String>>));

    let waiter_ui = waiter.clone();
    let done_ui = done_flag.clone();
    let cancel_ui = cancel.clone();
    glib::spawn_future_local(async move {
        loop {
            while let Ok(batch) = batch_rx.try_recv() {
                if !cancel_ui.load(Ordering::Relaxed) && !batch.is_empty() {
                    on_batch(batch);
                }
            }
            if let Some(res) = done_ui.lock().ok().and_then(|mut g| g.take()) {
                while let Ok(batch) = batch_rx.try_recv() {
                    if !cancel_ui.load(Ordering::Relaxed) && !batch.is_empty() {
                        on_batch(batch);
                    }
                }
                if !cancel_ui.load(Ordering::Relaxed) {
                    on_done(res);
                }
                break;
            }
            let (tx, rx) = futures_channel::oneshot::channel();
            {
                let Ok(mut w) = waiter_ui.lock() else {
                    break;
                };
                if done_ui.lock().ok().is_some_and(|g| g.is_some()) {
                    continue;
                }
                *w = Some(tx);
            }
            if let Ok(batch) = batch_rx.try_recv() {
                if !cancel_ui.load(Ordering::Relaxed) && !batch.is_empty() {
                    on_batch(batch);
                }
                continue;
            }
            if rx.await.is_err() {
                break;
            }
        }
    });

    let waiter_worker = waiter.clone();
    let done_worker = done_flag.clone();
    let notify = move || {
        if let Ok(mut w) = waiter_worker.lock() {
            if let Some(tx) = w.take() {
                let _ = tx.send(());
            }
        }
    };

    std::thread::spawn(move || {
        let res = search_worker(&root, &query, show_hidden, &cancel, |batch| {
            let _ = batch_tx.send(batch);
            notify();
        });
        if let Ok(mut d) = done_worker.lock() {
            *d = Some(res);
        }
        notify();
    });
}

fn should_skip_dir(path: &Path) -> bool {
    matches!(path.to_str(), Some("/proc" | "/sys" | "/dev" | "/run"))
}

pub fn search_worker(
    root: &Path,
    query: &str,
    show_hidden: bool,
    cancel: &AtomicBool,
    mut emit: impl FnMut(Vec<FileEntry>),
) -> Result<bool, String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Ok(false);
    }
    let mut ctx = ScanContext::new();
    let mut visited_dirs: HashSet<(u64, u64)> = HashSet::new();
    if let Ok(meta) = fs::metadata(root) {
        visited_dirs.insert((meta.dev(), meta.ino()));
    }
    let mut queue = VecDeque::from([root.to_path_buf()]);
    let mut count = 0usize;
    let mut batch = Vec::with_capacity(64);
    let mut last_emit = Instant::now();

    while let Some(dir) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".to_owned());
        }
        if dir != root && should_skip_dir(&dir) {
            continue;
        }
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for item in rd {
            if cancel.load(Ordering::Relaxed) {
                return Err("Cancelled".to_owned());
            }
            let Ok(item) = item else { continue };
            let os_name = item.file_name();
            let name = os_name.to_string_lossy();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let path = item.path();
            let (is_dir, is_symlink) = match item.file_type() {
                Ok(ft) => (ft.is_dir(), ft.is_symlink()),
                Err(_) => match fs::symlink_metadata(&path) {
                    Ok(m) => (m.is_dir(), m.file_type().is_symlink()),
                    Err(_) => continue,
                },
            };
            if is_dir
                && !is_symlink
                && !should_skip_dir(&path)
                && fs::symlink_metadata(&path)
                    .map(|m| visited_dirs.insert((m.dev(), m.ino())))
                    .unwrap_or(false)
            {
                queue.push_back(path.clone());
            }
            let lower = name.to_lowercase();
            if lower.contains(&query) {
                let rel_parent = dir
                    .strip_prefix(root)
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty());
                if let Some(entry) = ctx.build_entry(path, name.into_owned(), rel_parent, false) {
                    batch.push(entry);
                    count += 1;
                    if count >= LIMIT {
                        emit(std::mem::take(&mut batch));
                        return Ok(true);
                    }
                    if batch.len() >= 64 || last_emit.elapsed() >= Duration::from_millis(40) {
                        emit(std::mem::take(&mut batch));
                        last_emit = Instant::now();
                    }
                }
            }
        }
    }
    if !batch.is_empty() {
        emit(batch);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_search_respects_hidden_symlinks_and_cancel() {
        let root = std::env::temp_dir().join(format!("gavno-search-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join(".secret")).unwrap();
        fs::write(root.join("a/b/needle-one.txt"), b"x").unwrap();
        fs::write(root.join(".secret/needle-hidden.txt"), b"y").unwrap();
        let _ = std::os::unix::fs::symlink(&root, root.join("a/loop"));

        let mut found = Vec::new();
        let cancel = AtomicBool::new(false);
        let limited = search_worker(&root, "NEEDLE", false, &cancel, |mut b| {
            found.append(&mut b);
        })
        .unwrap();
        assert!(!limited);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "needle-one.txt");
        assert_eq!(found[0].subtitle.as_deref(), Some("a/b"));

        let mut found_hidden = Vec::new();
        search_worker(&root, "needle", true, &cancel, |mut b| {
            found_hidden.append(&mut b);
        })
        .unwrap();
        assert_eq!(found_hidden.len(), 2);

        let cancelled = AtomicBool::new(true);
        assert!(search_worker(&root, "needle", true, &cancelled, |_| {}).is_err());

        let _ = fs::remove_dir_all(&root);
    }
}
