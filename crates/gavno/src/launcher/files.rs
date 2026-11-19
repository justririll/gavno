//! File name search for the launcher.
//!
//! Everything here runs on a worker thread. The worker walks `$HOME` once
//! (breadth first, so shallow files come first), keeps the list of names for
//! `CACHE_TTL` and drops it afterwards, so the walk is paid for at most once a
//! minute and an idle launcher holds no memory. A query scans the cached list
//! and gives up after `MAX_HITS` matches or `SEARCH_BUDGET`.
use smithay::reexports::calloop::channel::Sender as ReplySender;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

pub const MAX_DEPTH: usize = 6;
pub const MAX_HITS: usize = 50;
pub const SEARCH_BUDGET: Duration = Duration::from_millis(150);
pub const CACHE_TTL: Duration = Duration::from_secs(60);
/// The walk stops here, however large the home directory is.
const WALK_BUDGET: Duration = Duration::from_millis(1500);
const MAX_ENTRIES: usize = 200_000;
/// Directories never worth entering.
const SKIPPED_DIRS: [&str; 2] = ["node_modules", "target"];

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

pub struct Request {
    pub seq: u64,
    /// Lower-case, trimmed.
    pub query: String,
}

pub struct Reply {
    pub seq: u64,
    pub hits: Vec<Hit>,
}

#[derive(Debug)]
pub struct Entry {
    lower: String,
    path: PathBuf,
    is_dir: bool,
    depth: u8,
}

/// Lists `root` breadth first: no hidden entries, no `node_modules`, `target`
/// or `.git`, no symlink following, at most `MAX_DEPTH` levels.
pub fn walk(root: &Path, budget: Duration) -> Vec<Entry> {
    let started = Instant::now();
    let mut out = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), 1usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for item in read.flatten() {
            let name = item.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = item.file_type() else { continue };
            let is_dir = kind.is_dir();
            let path = item.path();
            if is_dir && depth < MAX_DEPTH && !SKIPPED_DIRS.contains(&name) {
                queue.push_back((path.clone(), depth + 1));
            }
            if is_dir && SKIPPED_DIRS.contains(&name) {
                continue;
            }
            out.push(Entry { lower: name.to_lowercase(), path, is_dir, depth: depth as u8 });
        }
        if out.len() >= MAX_ENTRIES || started.elapsed() >= budget {
            break;
        }
    }
    out
}

/// Rank of a name for a lower-case query: lower is better.
fn rank(lower: &str, words: &[&str]) -> Option<u8> {
    if !words.iter().all(|w| lower.contains(w)) {
        return None;
    }
    let first = words[0];
    Some(if lower.starts_with(first) {
        0
    } else if lower.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(first)) {
        1
    } else {
        2
    })
}

/// Matches of `query` in `entries`, best first, at most `MAX_HITS`.
pub fn search(entries: &[Entry], query: &str, budget: Duration) -> Vec<Hit> {
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    let started = Instant::now();
    let mut found: Vec<(u8, u8, &Entry)> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(r) = rank(&entry.lower, &words) {
            found.push((r, entry.depth, entry));
            if found.len() >= MAX_HITS {
                break;
            }
        }
        if i % 4096 == 0 && started.elapsed() >= budget {
            break;
        }
    }
    found.sort_by(|a, b| (a.0, a.1, &a.2.lower).cmp(&(b.0, b.1, &b.2.lower)));
    found
        .into_iter()
        .map(|(_, _, e)| Hit {
            name: e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            path: e.path.clone(),
            is_dir: e.is_dir,
        })
        .collect()
}

/// Starts the worker; requests go in, replies come out on the event loop.
pub fn spawn(root: PathBuf, replies: ReplySender<Reply>) -> Option<mpsc::Sender<Request>> {
    let (tx, rx) = mpsc::channel::<Request>();
    std::thread::Builder::new()
        .name("launcher-files".into())
        .spawn(move || worker(&root, &rx, &replies))
        .ok()?;
    Some(tx)
}

fn worker(root: &Path, rx: &mpsc::Receiver<Request>, replies: &ReplySender<Reply>) {
    let mut index: Option<(Instant, Vec<Entry>)> = None;
    loop {
        let next = match &index {
            Some((built, _)) => match rx.recv_timeout(CACHE_TTL.saturating_sub(built.elapsed())) {
                Ok(request) => request,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    index = None;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(request) => request,
                Err(_) => return,
            },
        };
        // Only the newest query matters.
        let request = rx.try_iter().last().unwrap_or(next);
        if index.as_ref().is_none_or(|(built, _)| built.elapsed() >= CACHE_TTL) {
            index = Some((Instant::now(), walk(root, WALK_BUDGET)));
        }
        // Typing during the walk made this request stale; answer the latest.
        let request = rx.try_iter().last().unwrap_or(request);
        let hits = index.as_ref().map(|(_, e)| search(e, &request.query, SEARCH_BUDGET)).unwrap_or_default();
        if replies.send(Reply { seq: request.seq, hits }).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("gavno-files-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in [
            "docs/reports", ".hidden", "node_modules/pkg", "code/target", "code/src", "a/b/c/d/e/f/g/h",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [
            "docs/report-final.txt", "docs/reports/q1 report.pdf", ".hidden/report-secret.txt",
            "node_modules/pkg/report.js", "code/target/report.o", "code/src/reporter.rs", "report.md",
            ".dotreport", "a/b/c/d/e/deep-report.txt", "a/b/c/d/e/f/too-deep-report.txt",
        ] {
            std::fs::write(root.join(file), "x").unwrap();
        }
        root
    }

    fn names(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn walk_skips_hidden_and_build_directories_and_deep_files() {
        let root = tree();
        let entries = walk(&root, Duration::from_secs(5));
        let hits = search(&entries, "report", SEARCH_BUDGET);
        let found = names(&hits);
        assert!(found.contains(&"report.md"));
        assert!(found.contains(&"reporter.rs"));
        assert!(found.contains(&"deep-report.txt"));
        assert!(!found.contains(&"report-secret.txt"), "hidden directory");
        assert!(!found.contains(&".dotreport"), "hidden file");
        assert!(!found.contains(&"report.js"), "node_modules");
        assert!(!found.contains(&"report.o"), "target");
        assert!(!found.contains(&"too-deep-report.txt"), "deeper than {MAX_DEPTH}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ranking_prefers_prefix_then_shallow() {
        let root = tree();
        let entries = walk(&root, Duration::from_secs(5));
        let hits = search(&entries, "report", SEARCH_BUDGET);
        assert_eq!(hits[0].name, "report.md", "shallowest prefix match first");
        let last_prefix = hits.iter().rposition(|h| h.name.to_lowercase().starts_with("report")).unwrap();
        let first_other = hits.iter().position(|h| !h.name.to_lowercase().starts_with("report")).unwrap();
        assert!(last_prefix < first_other);
        assert!(hits.iter().any(|h| h.is_dir && h.name == "reports"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn several_words_must_all_match_and_results_are_capped() {
        let root = tree();
        let entries = walk(&root, Duration::from_secs(5));
        assert_eq!(names(&search(&entries, "q1 report", SEARCH_BUDGET)), ["q1 report.pdf"]);
        assert!(search(&entries, "   ", SEARCH_BUDGET).is_empty());
        let _ = std::fs::remove_dir_all(&root);
        let many: Vec<Entry> = (0..500)
            .map(|i| Entry { lower: format!("file{i}"), path: PathBuf::from(format!("/x/file{i}")), is_dir: false, depth: 1 })
            .collect();
        assert_eq!(search(&many, "file", SEARCH_BUDGET).len(), MAX_HITS);
    }
}
