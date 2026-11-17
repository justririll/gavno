//! Application chrome must never hold a new client frame behind an icon read/decode.
use super::*;
use smithay::reexports::calloop::channel::{self, Event};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    name: String,
    size: u32,
    theme: String,
    tint: bool,
    color: Option<[u8; 3]>,
}
#[derive(Default)]
struct Cache {
    ready: HashMap<Key, Option<MemoryRenderBuffer>>,
    pending: HashSet<Key>,
    generation: u64,
}
struct Job {
    key: Key,
    generation: u64,
}
struct Worker {
    jobs: mpsc::SyncSender<Job>,
    cache: Arc<Mutex<Cache>>,
}
static WORKER: OnceLock<Worker> = OnceLock::new();

impl Worker {
    fn new(reply: channel::Sender<()>) -> std::io::Result<Self> {
        let (jobs, rx) = mpsc::sync_channel::<Job>(128);
        let cache = Arc::new(Mutex::new(Cache::default()));
        let completed = cache.clone();
        std::thread::Builder::new()
            .name("gavno-app-icons".into())
            .spawn(move || {
                let mut paths: HashMap<(String, String), Option<PathBuf>> = HashMap::new();
                let mut path_generation = 0;
                for Job { key, generation } in rx {
                    if completed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .generation
                        != generation
                    {
                        let _ = reply.send(());
                        continue;
                    }
                    if path_generation != generation {
                        paths.clear();
                        path_generation = generation;
                    }
                    let path = paths
                        .entry((key.name.clone(), key.theme.clone()))
                        .or_insert_with(|| lookup_icon(&key.name, &key.theme))
                        .clone();
                    let buffer = path
                        .as_ref()
                        .and_then(|path| raster_color(path, key.size, key.tint, key.color));
                    // No I/O or decoding under the shared cache lock.
                    {
                        let mut cache = completed.lock().unwrap_or_else(|e| e.into_inner());
                        if cache.generation == generation {
                            cache.pending.remove(&key);
                            if cache.ready.len() >= 512 {
                                cache.ready.clear();
                            }
                            cache.ready.insert(key, buffer);
                        }
                    }
                    if paths.len() > 512 {
                        paths.clear();
                    }
                    let _ = reply.send(());
                }
            })?;
        Ok(Self { jobs, cache })
    }
    fn request(&self, key: Key) -> Option<Option<MemoryRenderBuffer>> {
        let generation = {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(buffer) = cache.ready.get(&key) {
                return Some(buffer.clone());
            }
            if !cache.pending.insert(key.clone()) {
                return None;
            }
            cache.generation
        };
        // A full queue never blocks input/rendering. Other completions wake a
        // redraw, at which point a request that did not fit can be retried.
        if self
            .jobs
            .try_send(Job {
                key: key.clone(),
                generation,
            })
            .is_err()
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.generation == generation {
                cache.pending.remove(&key);
            }
        }
        None
    }
    fn invalidate(&self) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.generation = cache.generation.wrapping_add(1);
        cache.ready.clear();
        cache.pending.clear();
    }
}
pub(crate) fn init(gv: &mut crate::Gavno) {
    if WORKER.get().is_some() {
        return;
    }
    let (tx, rx) = channel::channel();
    match Worker::new(tx) {
        Ok(worker) => {
            match gv.loop_handle.insert_source(rx, |event, _, gv| {
                let _stall = crate::perf::Stall::new("launch.app_icon_reply");
                if matches!(event, Event::Msg(())) {
                    gv.classic.taskbar.invalidate();
                    gv.queue_redraw_all();
                }
            }) {
                Ok(_) => {
                    let _ = WORKER.set(worker);
                }
                Err(error) => tracing::warn!(%error, "cannot watch application icons"),
            }
        }
        Err(error) => tracing::warn!(%error, "cannot start application icon worker"),
    }
}
/// Keep the existing configuration reload semantics without allowing an older
/// in-flight decode to repopulate the replacement cache.
pub(crate) fn invalidate() {
    if let Some(worker) = WORKER.get() {
        worker.invalidate();
    }
}
pub(super) fn enabled() -> bool {
    WORKER.get().is_some()
}
pub(super) fn request(
    name: &str,
    size: u32,
    theme: &str,
    tint: bool,
    color: Option<[u8; 3]>,
) -> Option<Option<MemoryRenderBuffer>> {
    WORKER.get()?.request(Key {
        name: name.into(),
        size,
        theme: theme.into(),
        tint,
        color,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_caches_missing_icons_and_decodes_without_a_renderer() {
        let (tx, rx) = channel::channel();
        let worker = Worker::new(tx).unwrap();
        let key = Key {
            name: "".into(),
            size: 16,
            theme: "Adwaita".into(),
            tint: false,
            color: None,
        };
        assert!(worker.request(key.clone()).is_none());
        // A completion, not a polling timer, wakes the compositor.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(buffer) = worker.request(key.clone()) {
                assert!(buffer.is_none());
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        while rx.try_recv().is_err() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let path = std::env::temp_dir().join(format!("gavno-app-icon-{}.svg", std::process::id()));
        std::fs::write(&path, "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\"><rect width=\"16\" height=\"16\" fill=\"red\"/></svg>").unwrap();
        let key = Key {
            name: path.to_string_lossy().into(),
            ..key
        };
        assert!(worker.request(key.clone()).is_none());
        loop {
            if let Some(buffer) = worker.request(key.clone()) {
                assert!(buffer.is_some());
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        std::fs::remove_file(path).unwrap();
        assert!(worker.request(key.clone()).unwrap().is_some());
        worker.invalidate();
        assert!(worker.request(key.clone()).is_none());
        loop {
            if let Some(buffer) = worker.request(key.clone()) {
                assert!(buffer.is_none());
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}
