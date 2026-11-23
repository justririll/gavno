use gtk::{gdk, prelude::*};
use lru::LruCache;
use std::{
    cell::RefCell,
    collections::HashSet,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
};

const MAX_QUEUE: usize = 64;
const CACHE_CAP: usize = 512;

pub fn cache_path(base: &Path, uri: &str, large: bool) -> PathBuf {
    base.join("thumbnails")
        .join(if large { "large" } else { "normal" })
        .join(format!("{:x}.png", md5::compute(uri.as_bytes())))
}

pub fn supported_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".webp", ".gif", ".svg", ".bmp", ".ico", ".tiff", ".tif",
        ".avif", ".jxl",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

struct Pixels {
    data: Vec<u8>,
    width: i32,
    height: i32,
    stride: usize,
    alpha: bool,
}

enum DecodeOutcome {
    Ok(Pixels),
    Failed,
    Cancelled,
}

struct Request {
    generation: u64,
    uri: String,
    path: PathBuf,
    modified: i64,
    size: u64,
    reply: futures_channel::oneshot::Sender<DecodeOutcome>,
}

struct QueueState {
    stack: Vec<Request>,
    closed: bool,
}

struct QueueGuard(Arc<(Mutex<QueueState>, Condvar)>);

impl Drop for QueueGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0 .0.lock() {
            state.closed = true;
            for req in state.stack.drain(..) {
                let _ = req.reply.send(DecodeOutcome::Cancelled);
            }
            self.0 .1.notify_all();
        }
    }
}

#[derive(Clone)]
pub struct Thumbnails {
    cache: Rc<RefCell<LruCache<String, gdk::Texture>>>,
    failed: Rc<RefCell<HashSet<String>>>,
    generation: Arc<AtomicU64>,
    queue: Arc<(Mutex<QueueState>, Condvar)>,
    _guard: Rc<QueueGuard>,
}

impl Default for Thumbnails {
    fn default() -> Self {
        Self::new()
    }
}

impl Thumbnails {
    pub fn new() -> Self {
        let generation = Arc::new(AtomicU64::new(1));
        let queue = Arc::new((
            Mutex::new(QueueState {
                stack: Vec::with_capacity(MAX_QUEUE),
                closed: false,
            }),
            Condvar::new(),
        ));
        for _ in 0..2 {
            let queue = queue.clone();
            let generation = generation.clone();
            std::thread::spawn(move || worker_loop(queue, generation));
        }
        Self {
            cache: Rc::new(RefCell::new(LruCache::new(
                NonZeroUsize::new(CACHE_CAP).expect("non-zero"),
            ))),
            failed: Rc::new(RefCell::new(HashSet::new())),
            generation,
            queue: queue.clone(),
            _guard: Rc::new(QueueGuard(queue)),
        }
    }

    /// Cancels all pending off-screen/previous-folder thumbnail work immediately.
    pub fn invalidate_pending(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut state) = self.queue.0.lock() {
            for req in state.stack.drain(..) {
                let _ = req.reply.send(DecodeOutcome::Cancelled);
            }
        }
    }

    pub async fn get(
        &self,
        path: &Path,
        uri: &str,
        modified: i64,
        size: u64,
    ) -> Option<gdk::Texture> {
        let key = format!("{uri}:{modified}:{size}");
        if self.failed.borrow().contains(&key) {
            return None;
        }
        if let Some(texture) = self.cache.borrow_mut().get(&key).cloned() {
            return Some(texture);
        }
        let gen = self.generation.load(Ordering::Relaxed);
        let (reply, result) = futures_channel::oneshot::channel();
        {
            let Ok(mut state) = self.queue.0.lock() else {
                return None;
            };
            if state.closed {
                return None;
            }
            if state.stack.len() >= MAX_QUEUE {
                let dropped = state.stack.remove(0);
                let _ = dropped.reply.send(DecodeOutcome::Cancelled);
            }
            state.stack.push(Request {
                generation: gen,
                uri: uri.to_owned(),
                path: path.to_path_buf(),
                modified,
                size,
                reply,
            });
            self.queue.1.notify_one();
        }
        let p = match result.await {
            Ok(DecodeOutcome::Ok(p)) => p,
            Ok(DecodeOutcome::Failed) => {
                let mut failed = self.failed.borrow_mut();
                if failed.len() >= 1024 {
                    failed.clear();
                }
                failed.insert(key);
                return None;
            }
            Ok(DecodeOutcome::Cancelled) | Err(_) => return None,
        };
        let texture: gdk::Texture = gdk::MemoryTexture::new(
            p.width,
            p.height,
            if p.alpha {
                gdk::MemoryFormat::R8g8b8a8
            } else {
                gdk::MemoryFormat::R8g8b8
            },
            &glib::Bytes::from_owned(p.data),
            p.stride,
        )
        .upcast();
        self.cache.borrow_mut().put(key, texture.clone());
        Some(texture)
    }
}

fn worker_loop(queue: Arc<(Mutex<QueueState>, Condvar)>, generation: Arc<AtomicU64>) {
    loop {
        let req = {
            let Ok(mut state) = queue.0.lock() else {
                return;
            };
            while state.stack.is_empty() && !state.closed {
                state = match queue.1.wait(state) {
                    Ok(s) => s,
                    Err(_) => return,
                };
            }
            if state.closed && state.stack.is_empty() {
                return;
            }
            state.stack.pop()
        };
        let Some(req) = req else { continue };
        if req.generation != generation.load(Ordering::Relaxed) {
            let _ = req.reply.send(DecodeOutcome::Cancelled);
            continue;
        }
        let outcome = match decode(&req) {
            Ok(pixels) => DecodeOutcome::Ok(pixels),
            Err(_) => DecodeOutcome::Failed,
        };
        let _ = req.reply.send(outcome);
    }
}

const MAX_THUMB_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_THUMB_DIM: i32 = 16_384;
const MAX_THUMB_PIXELS: i64 = 100_000_000;

fn decode(req: &Request) -> Result<Pixels, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let base = glib::user_cache_dir();
    let mtime_str = req.modified.to_string();
    let size_str = req.size.to_string();
    let mut found = None;
    for large in [true, false] {
        let path = cache_path(&base, &req.uri, large);
        if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file(&path) {
            let size_ok = pixbuf
                .option("tEXt::Thumb::Size")
                .is_none_or(|s| s.as_str() == size_str);
            let valid = pixbuf.option("tEXt::Thumb::URI").as_deref() == Some(req.uri.as_str())
                && pixbuf.option("tEXt::Thumb::MTime").as_deref() == Some(mtime_str.as_str())
                && size_ok;
            if valid {
                found = Some(pixbuf);
                break;
            }
        }
    }
    let pixbuf = match found {
        Some(p) => p,
        None => {
            let meta = std::fs::metadata(&req.path).map_err(|e| e.to_string())?;
            if !meta.file_type().is_file() || meta.len() > MAX_THUMB_FILE_BYTES {
                return Err("File too large or unsupported for thumbnail".into());
            }
            if let Some((_, w, h)) = gdk_pixbuf::Pixbuf::file_info(&req.path) {
                if w <= 0
                    || h <= 0
                    || w > MAX_THUMB_DIM
                    || h > MAX_THUMB_DIM
                    || (w as i64) * (h as i64) > MAX_THUMB_PIXELS
                {
                    return Err("Image dimensions exceed thumbnail limit".into());
                }
            }
            let original = gdk_pixbuf::Pixbuf::from_file_at_scale(&req.path, 256, 256, true)
                .map_err(|e| e.to_string())?;
            let p = original.apply_embedded_orientation().unwrap_or(original);
            if !req.path.starts_with(base.join("thumbnails")) {
                let target = cache_path(&base, &req.uri, true);
                if let Some(parent) = target.parent() {
                    let _ = std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(parent);
                }
                let temp = target.with_extension(format!(
                    "{}.{:?}.tmp",
                    std::process::id(),
                    std::thread::current().id()
                ));
                if std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .mode(0o600)
                    .open(&temp)
                    .is_ok()
                {
                    if p.savev(
                        &temp,
                        "png",
                        &[
                            ("tEXt::Thumb::URI", &req.uri),
                            ("tEXt::Thumb::MTime", &mtime_str),
                            ("tEXt::Thumb::Size", &size_str),
                            ("tEXt::Software", "gavno-files"),
                        ],
                    )
                    .is_ok()
                    {
                        if std::fs::rename(&temp, &target).is_err() {
                            let _ = std::fs::remove_file(&temp);
                        }
                    } else {
                        let _ = std::fs::remove_file(&temp);
                    }
                }
            }
            p
        }
    };
    Ok(Pixels {
        data: pixbuf.read_pixel_bytes().to_vec(),
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: pixbuf.rowstride() as usize,
        alpha: pixbuf.has_alpha(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freedesktop_md5() {
        assert_eq!(
            cache_path(Path::new("/cache"), "file:///home/user/image.png", false),
            PathBuf::from("/cache/thumbnails/normal/e47e29c02f69c7dd185f87f51d43a326.png")
        );
        assert_eq!(
            cache_path(Path::new("/cache"), "file:///tmp/a%20b.png", true)
                .file_name()
                .and_then(|s| s.to_str()),
            Some("f2584ab78dd95a88bd0d3f0ecaee7a8c.png")
        );
    }

    #[test]
    fn supported_extensions() {
        assert!(supported_name("PHOTO.PNG"));
        assert!(supported_name("shot.webp"));
        assert!(supported_name("diagram.svg"));
        assert!(!supported_name("main.rs"));
    }
}
