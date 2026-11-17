//! Application chrome must never hold a new client frame behind an icon read/decode.
use super::*;
use smithay::reexports::calloop::channel::{self, Event};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex, OnceLock},
};

const CAPACITY: usize = 512;
const WORK_CAPACITY: usize = 128;
#[derive(Clone, Copy)]
pub(crate) enum Consumer {
    Chrome = 1,
    Launcher = 2,
    Start = 4,
    Panel = 8,
}
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
    pending: HashMap<Key, u8>,
    generation: u64,
    queue: VecDeque<Job>,
    active: bool,
    shutdown: bool,
    retry_consumers: u8,
}
pub(crate) struct Delivery {
    consumers: u8,
    generation: u64,
}
struct Job {
    key: Key,
    generation: u64,
}
struct Worker {
    wake: Arc<Condvar>,
    cache: Arc<Mutex<Cache>>,
}
static WORKER: OnceLock<Worker> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_WORKER: std::cell::RefCell<Option<Worker>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(crate) fn with_test_worker<R>(
    lookup: impl FnMut(&str, &str) -> Option<PathBuf> + Send + 'static,
    decode: impl FnMut(&Path, u32, bool, Option<[u8; 3]>) -> Option<MemoryRenderBuffer> + Send + 'static,
    run: impl FnOnce(&channel::Channel<Delivery>) -> R,
) -> R {
    let (tx, rx) = channel::channel();
    TEST_WORKER
        .with(|slot| *slot.borrow_mut() = Some(Worker::with_loaders(tx, lookup, decode).unwrap()));
    let result = run(&rx);
    TEST_WORKER.with(|slot| slot.borrow_mut().take());
    result
}
#[cfg(test)]
pub(crate) fn wait_delivery(rx: &channel::Channel<Delivery>) -> Delivery {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(delivery) = rx.try_recv() {
            return delivery;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "icon worker did not deliver"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

impl Worker {
    fn new(reply: channel::Sender<Delivery>) -> std::io::Result<Self> {
        Self::with_loaders(reply, lookup_icon, raster_color)
    }
    fn with_loaders(
        reply: channel::Sender<Delivery>,
        mut lookup: impl FnMut(&str, &str) -> Option<PathBuf> + Send + 'static,
        mut decode: impl FnMut(&Path, u32, bool, Option<[u8; 3]>) -> Option<MemoryRenderBuffer>
            + Send
            + 'static,
    ) -> std::io::Result<Self> {
        let cache = Arc::new(Mutex::new(Cache::default()));
        let wake = Arc::new(Condvar::new());
        let completed = cache.clone();
        let worker_wake = wake.clone();
        std::thread::Builder::new()
            .name("gavno-app-icons".into())
            .spawn(move || {
                let mut paths: HashMap<(String, String), Option<PathBuf>> = HashMap::new();
                // Aliases of the same path share their decoded pixels. This cache
                // survives panel closure; theme invalidation clears it lazily.
                let mut rasters: HashMap<
                    (PathBuf, u32, bool, Option<[u8; 3]>),
                    Option<MemoryRenderBuffer>,
                > = HashMap::new();
                let mut path_generation = 0;
                loop {
                    let Job { key, generation } = {
                        let mut cache = completed.lock().unwrap_or_else(|e| e.into_inner());
                        while cache.queue.is_empty() && !cache.shutdown {
                            cache = worker_wake.wait(cache).unwrap_or_else(|e| e.into_inner());
                        }
                        if cache.shutdown {
                            break;
                        }
                        cache.active = true;
                        cache.queue.pop_front().unwrap()
                    };
                    if path_generation != generation {
                        paths.clear();
                        rasters.clear();
                        path_generation = generation;
                    }
                    let current_generation = completed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .generation
                        == generation;
                    let path = if current_generation {
                        paths
                            .entry((key.name.clone(), key.theme.clone()))
                            .or_insert_with(|| lookup(&key.name, &key.theme))
                            .clone()
                    } else {
                        None
                    };
                    let mut raster_overflow = false;
                    let buffer = path
                        .as_ref()
                        .filter(|_| {
                            completed
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .generation
                                == generation
                        })
                        .and_then(|path| {
                            let raster_key = (path.clone(), key.size, key.tint, key.color);
                            if let Some(buffer) = rasters.get(&raster_key) {
                                return buffer.clone();
                            }
                            let buffer = decode(path, key.size, key.tint, key.color);
                            if rasters.len() >= CAPACITY {
                                rasters.clear();
                                raster_overflow = true;
                            }
                            rasters.insert(raster_key, buffer.clone());
                            buffer
                        });
                    // No I/O or decoding under the shared cache lock.
                    let (consumers, delivery_generation) = {
                        let mut cache = completed.lock().unwrap_or_else(|e| e.into_inner());
                        cache.active = false;
                        // Capacity-rejected visible consumers retry on this
                        // delivery, including when an old active job finishes.
                        let mut consumers = std::mem::take(&mut cache.retry_consumers);
                        if cache.generation == generation {
                            consumers |= cache.pending.remove(&key).unwrap_or_default();
                            if raster_overflow || cache.ready.len() >= CAPACITY {
                                cache.ready.clear();
                            }
                            cache.ready.insert(key, buffer);
                        }
                        (consumers, cache.generation)
                    };
                    if paths.len() >= CAPACITY {
                        paths.clear();
                    }
                    if consumers != 0 {
                        let _ = reply.send(Delivery {
                            consumers,
                            generation: delivery_generation,
                        });
                    }
                }
            })?;
        Ok(Self { wake, cache })
    }
    fn request(&self, key: Key, consumer: Consumer) -> Option<Option<MemoryRenderBuffer>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(buffer) = cache.ready.get(&key) {
            return Some(buffer.clone());
        }
        if let Some(consumers) = cache.pending.get_mut(&key) {
            *consumers |= consumer as u8;
            return None;
        }
        // Count the active operation even if theme invalidation superseded it.
        // Rejected requests retain only consumer bits, never unbounded keys.
        if cache.queue.len() + usize::from(cache.active) >= WORK_CAPACITY {
            cache.retry_consumers |= consumer as u8;
            return None;
        }
        let generation = cache.generation;
        cache.pending.insert(key.clone(), consumer as u8);
        cache.queue.push_back(Job { key, generation });
        self.wake.notify_one();
        None
    }
    fn invalidate(&self) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.generation = cache.generation.wrapping_add(1);
        cache.ready.clear();
        cache.pending.clear();
        cache.queue.clear();
    }
    fn generation(&self) -> u64 {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown = true;
        self.wake.notify_one();
    }
}
impl Delivery {
    pub(crate) fn apply(&self, gv: &mut crate::Gavno) {
        let (chrome, launcher, start) = self.targets(gv);
        if chrome {
            gv.classic.taskbar.invalidate();
            gv.queue_redraw_all();
        } else {
            if let Some(output) = &launcher {
                crate::backend::queue_redraw_output(gv, output);
            }
            if let Some(output) = start.filter(|output| Some(output) != launcher.as_ref()) {
                crate::backend::queue_redraw_output(gv, &output);
            }
        }
    }
    fn targets(
        &self,
        gv: &mut crate::Gavno,
    ) -> (
        bool,
        Option<smithay::output::Output>,
        Option<smithay::output::Output>,
    ) {
        if self.generation != generation() {
            return (false, None, None);
        }
        let launcher = (self.consumers & Consumer::Launcher as u8 != 0)
            .then(|| gv.launcher.icon_ready())
            .flatten();
        let start = (self.consumers & Consumer::Start as u8 != 0)
            .then(|| gv.classic.start.icon_ready())
            .flatten();
        (
            self.consumers & Consumer::Chrome as u8 != 0
                || (self.consumers & Consumer::Panel as u8 != 0 && gv.panel_controls_open()),
            launcher,
            start,
        )
    }
}
pub(crate) fn init(gv: &mut crate::Gavno) {
    if WORKER.get().is_some() {
        return;
    }
    let (tx, rx) = channel::channel();
    match Worker::new(tx) {
        Ok(worker) => match gv.loop_handle.insert_source(rx, |event, _, gv| {
            let _stall = crate::perf::Stall::new("launch.app_icon_reply");
            if let Event::Msg(delivery) = event {
                delivery.apply(gv);
            }
        }) {
            Ok(_) => {
                let _ = WORKER.set(worker);
            }
            Err(error) => tracing::warn!(%error, "cannot watch application icons"),
        },
        Err(error) => tracing::warn!(%error, "cannot start application icon worker"),
    }
}
/// Older in-flight decodes cannot repopulate the replacement cache.
pub(crate) fn invalidate() {
    super::paths().clear();
    if let Some(worker) = WORKER.get() {
        worker.invalidate();
    }
}
pub(crate) fn generation() -> u64 {
    #[cfg(test)]
    if let Some(generation) =
        TEST_WORKER.with(|slot| slot.borrow().as_ref().map(Worker::generation))
    {
        return generation;
    }
    WORKER.get().map_or(0, Worker::generation)
}
pub(super) fn request(
    name: &str,
    size: u32,
    theme: &str,
    tint: bool,
    color: Option<[u8; 3]>,
    consumer: Consumer,
) -> Option<Option<MemoryRenderBuffer>> {
    let key = Key {
        name: name.into(),
        size,
        theme: theme.into(),
        tint,
        color,
    };
    #[cfg(test)]
    if let Some(result) = TEST_WORKER.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|worker| worker.request(key.clone(), consumer))
    }) {
        return result;
    }
    WORKER.get()?.request(key, consumer)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(crate) fn pixels(
        _: &Path,
        size: u32,
        _: bool,
        _: Option<[u8; 3]>,
    ) -> Option<MemoryRenderBuffer> {
        Some(MemoryRenderBuffer::from_slice(
            &vec![255; (size * size * 4) as usize],
            Fourcc::Abgr8888,
            (size as i32, size as i32),
            1,
            Transform::Normal,
            None,
        ))
    }
    #[test]
    fn requests_never_lookup_on_caller_and_aliases_share_bounded_decodes() {
        let caller = std::thread::current().id();
        let decodes = Arc::new(AtomicUsize::new(0));
        let count = decodes.clone();
        let (tx, rx) = channel::channel();
        let worker = Worker::with_loaders(
            tx,
            move |_, _| {
                assert_ne!(caller, std::thread::current().id());
                Some(PathBuf::from("/injected/icon.svg"))
            },
            move |path, size, tint, color| {
                assert_ne!(caller, std::thread::current().id());
                count.fetch_add(1, Ordering::Relaxed);
                pixels(path, size, tint, color)
            },
        )
        .unwrap();
        let mut key = Key {
            name: "one".into(),
            size: 16,
            theme: "test".into(),
            tint: false,
            color: None,
        };
        assert!(worker.request(key.clone(), Consumer::Start).is_none());
        assert_eq!(wait_delivery(&rx).consumers, Consumer::Start as u8);
        assert!(worker
            .request(key.clone(), Consumer::Start)
            .unwrap()
            .is_some());
        key.name = "alias".into();
        assert!(worker.request(key.clone(), Consumer::Start).is_none());
        wait_delivery(&rx);
        assert_eq!(decodes.load(Ordering::Relaxed), 1);
        // Completed entries survive panel destruction; a new Icons uses them.
        assert!(worker
            .request(key.clone(), Consumer::Start)
            .unwrap()
            .is_some());
        worker.invalidate();
        assert!(worker.request(key.clone(), Consumer::Start).is_none());
        wait_delivery(&rx);
        assert_eq!(decodes.load(Ordering::Relaxed), 2);
        for n in 0..=CAPACITY {
            key.name = format!("icon-{n}");
            worker.request(key.clone(), Consumer::Launcher);
            wait_delivery(&rx);
        }
        assert!(worker.cache.lock().unwrap().ready.len() <= CAPACITY);
    }
    #[test]
    fn blocked_work_is_bounded_superseded_and_retried_by_delivery() {
        for block_decode in [false, true] {
            let (tx, rx) = channel::channel();
            let (started, begin) = std::sync::mpsc::channel();
            let (release, gate) = std::sync::mpsc::channel();
            let (mut lookup_gate, mut decode_gate) = if block_decode {
                (None, Some(gate))
            } else {
                (Some(gate), None)
            };
            let lookup_started = started.clone();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let lookups = calls.clone();
            let worker = Worker::with_loaders(
                tx,
                move |name, _| {
                    lookups.lock().unwrap().push(name.to_owned());
                    if let Some(gate) = lookup_gate.take() {
                        lookup_started.send(()).unwrap();
                        gate.recv().unwrap();
                    }
                    Some(PathBuf::from(name))
                },
                move |path, size, tint, color| {
                    if let Some(gate) = decode_gate.take() {
                        started.send(()).unwrap();
                        gate.recv().unwrap();
                    }
                    pixels(path, size, tint, color)
                },
            )
            .unwrap();
            let key = |name: String| Key {
                name,
                size: 16,
                theme: "test".into(),
                tint: false,
                color: None,
            };
            worker.request(key("active".into()), Consumer::Chrome);
            begin
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            // The active operation cannot be cancelled, but all queued work
            // from older generations must disappear immediately.
            for generation in 0..32 {
                worker.invalidate();
                for n in 0..WORK_CAPACITY * 4 {
                    worker.request(
                        key(format!("generation-{generation}-{n}")),
                        Consumer::Launcher,
                    );
                }
                let cache = worker.cache.lock().unwrap();
                assert_eq!(cache.queue.len() + usize::from(cache.active), WORK_CAPACITY);
                assert!(cache.pending.len() <= WORK_CAPACITY);
                assert!(cache
                    .queue
                    .iter()
                    .all(|job| job.generation == cache.generation));
            }
            let visible = key("visible-retry".into());
            assert!(worker.request(visible.clone(), Consumer::Start).is_none());
            assert!(!worker.cache.lock().unwrap().pending.contains_key(&visible));
            release.send(()).unwrap();
            let freed = wait_delivery(&rx);
            assert_eq!(freed.generation, worker.generation());
            assert_ne!(
                freed.consumers & Consumer::Start as u8,
                0,
                "a completion must redraw the rejected visible consumer"
            );
            loop {
                if let Some(buffer) = worker.request(visible.clone(), Consumer::Start) {
                    assert!(buffer.is_some());
                    break;
                }
                wait_delivery(&rx);
            }
            assert!(calls.lock().unwrap().iter().all(|name| name == "active"
                || name == "visible-retry"
                || name.starts_with("generation-31-")));
        }
    }

    #[test]
    fn delivery_targets_only_visible_panels_and_drops_stale_generations() {
        let mut h = crate::state::tests::Harness::new();
        let output = smithay::output::Output::new(
            "icons".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        h.gv.space.map_output(&output, (0, 0));
        h.gv.toggle_launcher();
        let delivery = Delivery {
            consumers: Consumer::Launcher as u8,
            generation: generation(),
        };
        assert_eq!(delivery.targets(&mut h.gv), (false, Some(output), None));
        h.gv.close_launcher();
        // Fade-out remains visible and needs its icons too.
        h.gv.launcher = Default::default();
        assert_eq!(delivery.targets(&mut h.gv), (false, None, None));
        let stale = Delivery {
            generation: generation().wrapping_add(1),
            ..delivery
        };
        assert_eq!(stale.targets(&mut h.gv), (false, None, None));
    }
}

#[cfg(test)]
#[test]
fn closed_panel_icon_delivery_does_not_redraw() {
    let mut h = crate::state::tests::Harness::new();
    let delivery = Delivery { consumers: Consumer::Panel as u8, generation: generation() };
    assert!(!delivery.targets(&mut h.gv).0);
}
