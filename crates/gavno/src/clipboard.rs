//! Clipboard history. Selections that clients set through the data device are
//! read once through a pipe on a worker thread and kept in memory only. Choosing
//! an entry makes the compositor own the selection and serve the bytes itself.
//! Nothing runs, and nothing is registered, until a client changes the
//! selection; the picker lives in `clipboard_picker.rs`.
use std::{
    collections::{hash_map::DefaultHasher, VecDeque},
    hash::{Hash, Hasher},
    io::Read,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::element::memory::MemoryRenderBuffer,
    reexports::calloop::{channel, LoopHandle},
    wayland::selection::{
        data_device::{request_data_device_client_selection, set_data_device_selection},
        SelectionSource, SelectionTarget,
    },
};

use crate::Gavno;

/// Largest text entry in bytes.
pub const MAX_TEXT: usize = 1 << 20;
/// Largest image entry in bytes.
pub const MAX_IMAGE: usize = 8 << 20;
/// A client that has not delivered everything by then is ignored.
const READ_TIMEOUT: Duration = Duration::from_secs(3);
/// Thumbnails fit this box, in physical pixels.
const THUMB_W: u32 = 112;
const THUMB_H: u32 = 64;
const HINT_MIME: &str = "x-kde-passwordManagerHint";
const SECRET_MIME: &str = "application/x-gavno-secret";
const TEXT_MIMES: [&str; 2] = ["text/plain;charset=utf-8", "text/plain"];
const IMAGE_MIME: &str = "image/png";
/// What the compositor offers when it owns a text selection.
const OFFER_TEXT: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/// Bytes the compositor serves for the selection it owns.
#[derive(Debug, Clone)]
pub struct Payload(pub Arc<Vec<u8>>);

/// What to read from a new client selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub mime: &'static str,
    pub image: bool,
    /// The offer carries a password manager hint that must be read first.
    pub hint: bool,
}

/// Picks the mime type to record, or `None` for selections that must never be
/// recorded (explicit secrets) or carry nothing recordable.
pub fn plan(mimes: &[String]) -> Option<Plan> {
    if mimes.iter().any(|m| m == SECRET_MIME) {
        return None;
    }
    let hint = mimes.iter().any(|m| m == HINT_MIME);
    if let Some(mime) = TEXT_MIMES.iter().find(|t| mimes.iter().any(|m| m == **t)) {
        return Some(Plan { mime, image: false, hint });
    }
    mimes.iter().any(|m| m == IMAGE_MIME).then_some(Plan { mime: IMAGE_MIME, image: true, hint })
}

pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub buffer: MemoryRenderBuffer,
}

pub struct ImageData {
    pub png: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub thumb: Option<Thumb>,
}

pub enum Content {
    Text(String),
    Image(ImageData),
}

pub struct Entry {
    pub id: u64,
    pub content: Content,
    /// One line for the picker row.
    pub preview: String,
    /// Dim second line, empty when there is nothing to add.
    pub detail: String,
    /// Lower-case text the picker search matches against.
    pub haystack: String,
    hash: u64,
}

/// A finished read from the worker thread.
pub enum Captured {
    Text(String),
    Image { png: Vec<u8>, width: u32, height: u32, thumb: Option<(u32, u32, Vec<u8>)> },
}

fn hash_of(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Whitespace collapsed to single spaces, cut to `max` characters.
pub fn one_line(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for word in text.split_whitespace() {
        if count >= max {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
            count += 1;
        }
        for c in word.chars().take(max.saturating_sub(count)) {
            out.push(c);
            count += 1;
        }
    }
    out
}

fn human_size(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    } else {
        format!("{} KB", bytes.div_ceil(1024).max(1))
    }
}

/// Newest first, deduplicated, bounded.
#[derive(Default)]
pub struct History {
    entries: VecDeque<Entry>,
    next_id: u64,
    limit: usize,
}

impl History {
    pub fn with_limit(limit: usize) -> Self {
        Self { limit: limit.max(1), ..Default::default() }
    }
    pub fn entries(&self) -> &VecDeque<Entry> {
        &self.entries
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
        self.entries.truncate(self.limit);
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }
    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        self.entries.len() != before
    }
    /// Moves an existing entry to the front.
    pub fn promote(&mut self, id: u64) {
        if let Some(i) = self.entries.iter().position(|e| e.id == id) {
            if let Some(entry) = self.entries.remove(i) {
                self.entries.push_front(entry);
            }
        }
    }

    fn insert(&mut self, content: Content, hash: u64, preview: String, detail: String, haystack: String) -> u64 {
        // The same content moves to the front instead of repeating.
        let existing = self.entries.iter().position(|e| {
            e.hash == hash
                && match (&e.content, &content) {
                    (Content::Text(a), Content::Text(b)) => a == b,
                    (Content::Image(a), Content::Image(b)) => a.png == b.png,
                    _ => false,
                }
        });
        if let Some(i) = existing {
            let entry = self.entries.remove(i).unwrap();
            let id = entry.id;
            self.entries.push_front(entry);
            return id;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.entries.push_front(Entry { id, content, preview, detail, haystack, hash });
        self.entries.truncate(self.limit.max(1));
        id
    }

    /// Records text. Empty and oversized text is refused.
    pub fn push_text(&mut self, text: String) -> Option<u64> {
        if text.len() > MAX_TEXT || text.trim().is_empty() {
            return None;
        }
        let lines = text.lines().count();
        let chars = text.chars().count();
        let detail = if lines > 1 {
            format!("{lines} lines, {chars} characters")
        } else if chars > 90 {
            format!("{chars} characters")
        } else {
            String::new()
        };
        let preview = one_line(&text, 160);
        let haystack: String = text.chars().take(4096).collect::<String>().to_lowercase();
        let hash = hash_of(text.as_bytes());
        Some(self.insert(Content::Text(text), hash, preview, detail, haystack))
    }

    /// Records a PNG. Oversized or empty images are refused.
    pub fn push_image(&mut self, png: Vec<u8>, width: u32, height: u32, thumb: Option<(u32, u32, Vec<u8>)>) -> Option<u64> {
        if png.is_empty() || png.len() > MAX_IMAGE || width == 0 || height == 0 {
            return None;
        }
        let hash = hash_of(&png);
        let preview = format!("Image {width} x {height}");
        let detail = format!("PNG, {}", human_size(png.len()));
        let haystack = format!("image png {width}x{height} {width} x {height}");
        let thumb = thumb.filter(|(w, h, px)| *w > 0 && *h > 0 && px.len() == (*w * *h * 4) as usize).map(|(w, h, px)| Thumb {
            width: w,
            height: h,
            buffer: MemoryRenderBuffer::from_slice(
                &px,
                smithay::backend::allocator::Fourcc::Abgr8888,
                (w as i32, h as i32),
                1,
                smithay::utils::Transform::Normal,
                None,
            ),
        });
        let image = ImageData { png: Arc::new(png), width, height, thumb };
        Some(self.insert(Content::Image(image), hash, preview, detail, haystack))
    }

    pub fn push(&mut self, captured: Captured) -> Option<u64> {
        match captured {
            Captured::Text(text) => self.push_text(text),
            Captured::Image { png, width, height, thumb } => self.push_image(png, width, height, thumb),
        }
    }
}

/// Everything the clipboard feature keeps. It lives in the overlay state so the
/// renderer can draw the picker without new arguments.
pub struct Clipboard {
    pub history: History,
    pub picker: crate::clipboard_picker::Picker,
    tx: Option<channel::Sender<Captured>>,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self { history: History::with_limit(50), picker: Default::default(), tx: None }
    }
}

// ---- pipes -----------------------------------------------------------------

pub(crate) fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Safety: pipe2 just returned two fresh descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Waits until `fd` is ready for `events`; false on timeout or error.
fn wait_fd(fd: &impl AsRawFd, events: i16, deadline: Instant) -> bool {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events, revents: 0 };
        let ms = left.as_millis().clamp(1, 1000) as i32;
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n > 0 {
            return true;
        }
        if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Reads until EOF. `None` when more than `cap` bytes arrive or time runs out.
pub fn read_capped(fd: OwnedFd, cap: usize, deadline: Instant) -> Option<Vec<u8>> {
    let mut file = std::fs::File::from(fd);
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if !wait_fd(&file, libc::POLLIN, deadline) {
            return None;
        }
        match file.read(&mut chunk) {
            Ok(0) => return Some(out),
            Ok(n) => {
                if out.len() + n > cap {
                    return None;
                }
                out.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted || e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return None,
        }
    }
}

/// Writes all of `data`, giving up when the reader stalls.
pub fn write_all_timeout(fd: OwnedFd, data: &[u8], timeout: Duration) -> bool {
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let deadline = Instant::now() + timeout;
    let mut sent = 0;
    while sent < data.len() {
        if !wait_fd(&fd, libc::POLLOUT, deadline) {
            return false;
        }
        let n = unsafe { libc::write(fd.as_raw_fd(), data[sent..].as_ptr().cast(), data.len() - sent) };
        if n < 0 {
            if matches!(std::io::Error::last_os_error().kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock) {
                continue;
            }
            return false;
        }
        sent += n as usize;
    }
    true
}

// ---- worker ----------------------------------------------------------------

/// Downscales RGBA to `tw` x `th` by averaging up to 4 x 4 samples per target
/// pixel; cheap enough for the compositor thread.
pub fn downscale(rgba: &[u8], w: u32, h: u32, tw: u32, th: u32) -> Vec<u8> {
    let mut out = vec![0u8; (tw * th * 4) as usize];
    if w == 0 || h == 0 || tw == 0 || th == 0 || rgba.len() < (w * h * 4) as usize {
        return out;
    }
    for ty in 0..th {
        let y0 = (ty as u64 * h as u64 / th as u64) as u32;
        let y1 = (((ty + 1) as u64 * h as u64 / th as u64) as u32).max(y0 + 1).min(h);
        let ys = ((y1 - y0) / 4).max(1);
        for tx in 0..tw {
            let x0 = (tx as u64 * w as u64 / tw as u64) as u32;
            let x1 = (((tx + 1) as u64 * w as u64 / tw as u64) as u32).max(x0 + 1).min(w);
            let xs = ((x1 - x0) / 4).max(1);
            let mut sum = [0u32; 4];
            let mut n = 0;
            let mut y = y0;
            while y < y1 {
                let mut x = x0;
                while x < x1 {
                    let i = ((y * w + x) * 4) as usize;
                    for c in 0..4 {
                        sum[c] += rgba[i + c] as u32;
                    }
                    n += 1;
                    x += xs;
                }
                y += ys;
            }
            let o = ((ty * tw + tx) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (sum[c] / n.max(1)) as u8;
            }
        }
    }
    out
}

/// Size that fits `w` x `h` into the box without upscaling.
pub fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    let k = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    (((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1))
}

/// Decodes a PNG to RGBA, refusing anything over 64 megapixels.
fn decode_png(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    if info.width == 0 || info.height == 0 || (info.width as u64) * (info.height as u64) > 64_000_000 {
        return None;
    }
    let mut buf = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (frame.width, frame.height);
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => return None,
    };
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for px in buf[..frame.buffer_size()].chunks_exact(channels) {
        rgba.extend_from_slice(&match channels {
            4 => [px[0], px[1], px[2], px[3]],
            3 => [px[0], px[1], px[2], 255],
            2 => [px[0], px[0], px[0], px[1]],
            _ => [px[0], px[0], px[0], 255],
        });
    }
    Some((w, h, rgba))
}

/// Reads and validates one selection. Runs on a worker thread.
pub fn read_selection(plan: Plan, hint: Option<OwnedFd>, data: OwnedFd) -> Option<Captured> {
    let deadline = Instant::now() + READ_TIMEOUT;
    if let Some(hint) = hint {
        // Unreadable or secret: never record.
        let value = read_capped(hint, 64, deadline)?;
        if value.trim_ascii() == b"secret" {
            return None;
        }
    }
    if plan.image {
        let png = read_capped(data, MAX_IMAGE, deadline)?;
        let (width, height, rgba) = decode_png(&png)?;
        let (tw, th) = fit(width, height, THUMB_W, THUMB_H);
        let thumb = Some((tw, th, downscale(&rgba, width, height, tw, th)));
        Some(Captured::Image { png, width, height, thumb })
    } else {
        let bytes = read_capped(data, MAX_TEXT, deadline)?;
        let text = String::from_utf8(bytes).ok()?;
        Some(Captured::Text(text.trim_end_matches('\0').to_owned()))
    }
}

/// A calloop channel whose messages run `f` on the event loop. Created on first
/// use, so an unused feature registers nothing.
pub(crate) fn worker_channel<T: Send + 'static>(
    handle: &LoopHandle<'static, Gavno>,
    mut f: impl FnMut(T, &mut Gavno) + 'static,
) -> Option<channel::Sender<T>> {
    let (tx, rx) = channel::channel::<T>();
    handle
        .insert_source(rx, move |event, _, gv| {
            if let channel::Event::Msg(message) = event {
                f(message, gv);
            }
        })
        .ok()?;
    Some(tx)
}

// ---- compositor glue --------------------------------------------------------

impl Gavno {
    /// True while recording is on and nothing blocks it.
    fn clipboard_recording(&self) -> bool {
        self.config.clipboard.history && !self.protocols.locks.locked && !self.lock.internal() && self.greeter.is_none()
    }

    /// `SelectionHandler::new_selection`: queue a read for later; the selection
    /// is stored only after this callback returns.
    pub(crate) fn clipboard_new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>) {
        if ty != SelectionTarget::Clipboard || !self.clipboard_recording() {
            return;
        }
        let Some(source) = source else { return };
        let Some(plan) = plan(&source.mime_types()) else { return };
        self.loop_handle.insert_idle(move |gv| gv.clipboard_read(plan));
    }

    fn clipboard_read(&mut self, plan: Plan) {
        if !self.clipboard_recording() {
            return;
        }
        let Ok((data_read, data_write)) = pipe() else { return };
        if request_data_device_client_selection::<Gavno>(&self.seat, plan.mime.to_owned(), data_write).is_err() {
            return;
        }
        let mut hint_read = None;
        if plan.hint {
            let Ok((read, write)) = pipe() else { return };
            if request_data_device_client_selection::<Gavno>(&self.seat, HINT_MIME.to_owned(), write).is_err() {
                // The hint vanished with the selection: record nothing.
                return;
            }
            hint_read = Some(read);
        }
        if self.overlay.clipboard.tx.is_none() {
            self.overlay.clipboard.tx = worker_channel(&self.loop_handle, |captured: Captured, gv: &mut Gavno| gv.clipboard_install(captured));
        }
        let Some(tx) = self.overlay.clipboard.tx.clone() else { return };
        let _ = std::thread::Builder::new().name("gavno-clip-read".into()).spawn(move || {
            if let Some(captured) = read_selection(plan, hint_read, data_read) {
                let _ = tx.send(captured);
            }
        });
    }

    fn clipboard_install(&mut self, captured: Captured) {
        if !self.clipboard_recording() {
            return;
        }
        self.overlay.clipboard.history.push(captured);
        self.clipboard_changed();
    }

    /// Records the screenshot the compositor just copied.
    pub(crate) fn clipboard_record_image(&mut self, png: Vec<u8>, width: u32, height: u32, thumb: Option<(u32, u32, Vec<u8>)>) {
        if self.clipboard_recording() {
            self.overlay.clipboard.history.push_image(png, width, height, thumb);
            self.clipboard_changed();
        }
    }

    fn clipboard_changed(&mut self) {
        if self.overlay.clipboard.picker.open {
            self.overlay.clipboard.picker.refilter(&self.overlay.clipboard.history);
            self.queue_redraw_all();
        }
    }

    /// Applies `[clipboard]` after a reload: turning history off drops it.
    pub(crate) fn clipboard_config_changed(&mut self) {
        if !self.config.clipboard.history {
            self.overlay.clipboard.history.clear();
            self.close_clipboard_picker();
        }
        self.overlay.clipboard.history.set_limit(self.config.clipboard.history_size as usize);
    }

    /// Makes the compositor own the selection with `payload`.
    pub(crate) fn own_selection(&mut self, mimes: &[&str], payload: Payload) {
        set_data_device_selection::<Gavno>(&self.display_handle, &self.seat, mimes.iter().map(|m| (*m).to_owned()).collect(), payload);
    }

    pub(crate) fn own_text_selection(&mut self, text: &str) {
        self.own_selection(&OFFER_TEXT, Payload(Arc::new(text.as_bytes().to_vec())));
    }

    pub(crate) fn own_png_selection(&mut self, png: Arc<Vec<u8>>) {
        self.own_selection(&[IMAGE_MIME], Payload(png));
    }

    /// Ctrl+V through the normal key path, once the selection is in place.
    pub(crate) fn inject_paste(&mut self) {
        use crate::ipc::debug_input::events::{Backend, Key};
        use smithay::backend::input::{InputEvent, KeyState};
        const CTRL: u32 = 29;
        const V: u32 = 47;
        let time = self.start_time.elapsed().as_micros() as u64;
        for (code, state) in [(CTRL, KeyState::Pressed), (V, KeyState::Pressed), (V, KeyState::Released), (CTRL, KeyState::Released)] {
            self.process_input_event::<Backend>(InputEvent::Keyboard { event: Key { time, code, state } });
        }
    }
}

/// `SelectionHandler::send_selection` for compositor-owned data.
pub(crate) fn serve(fd: OwnedFd, payload: &Payload) {
    let data = payload.0.clone();
    let _ = std::thread::Builder::new().name("gavno-clip-send".into()).spawn(move || {
        write_all_timeout(fd, &data, Duration::from_secs(5));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn plan_prefers_text_then_image_and_skips_secrets() {
        let p = plan(&mimes(&["text/html", "text/plain;charset=utf-8", "text/plain", "image/png"])).unwrap();
        assert_eq!((p.mime, p.image, p.hint), ("text/plain;charset=utf-8", false, false));
        assert_eq!(plan(&mimes(&["text/plain"])).unwrap().mime, "text/plain");
        let p = plan(&mimes(&["image/png", "text/html"])).unwrap();
        assert!(p.image);
        assert_eq!(plan(&mimes(&["text/html", "image/jpeg"])), None);
        // Explicit secrets are never recorded, whatever else is offered.
        assert_eq!(plan(&mimes(&["text/plain", "application/x-gavno-secret"])), None);
        // The KDE hint asks for a read of its value before anything is kept.
        assert!(plan(&mimes(&["text/plain", "x-kde-passwordManagerHint"])).unwrap().hint);
    }

    fn feed(data: &[u8]) -> OwnedFd {
        let (read, write) = pipe().unwrap();
        let data = data.to_vec();
        std::thread::spawn(move || {
            write_all_timeout(write, &data, Duration::from_secs(5));
        });
        read
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(3)
    }

    #[test]
    fn reads_are_capped_and_time_limited() {
        assert_eq!(read_capped(feed(b"hello"), 16, soon()).unwrap(), b"hello");
        assert_eq!(read_capped(feed(&[b'x'; 17]), 16, soon()), None);
        assert_eq!(read_capped(feed(&vec![b'x'; 16]), 16, soon()).unwrap().len(), 16);
        // A client that never writes and never closes cannot hang the worker.
        let (read, _write) = pipe().unwrap();
        let start = Instant::now();
        assert_eq!(read_capped(read, 16, Instant::now() + Duration::from_millis(80)), None);
        assert!(start.elapsed() < Duration::from_secs(2));
        // Larger than the pipe buffer still arrives intact.
        let big = vec![7u8; 300_000];
        assert_eq!(read_capped(feed(&big), MAX_TEXT, soon()).unwrap(), big);
    }

    #[test]
    fn password_manager_hint_blocks_recording() {
        let text = Plan { mime: "text/plain", image: false, hint: true };
        assert!(read_selection(text, Some(feed(b"secret")), feed(b"hunter2")).is_none());
        assert!(read_selection(text, Some(feed(b"secret\n")), feed(b"hunter2")).is_none());
        match read_selection(text, Some(feed(b"")), feed(b"visible")) {
            Some(Captured::Text(t)) => assert_eq!(t, "visible"),
            _ => panic!("a non-secret hint must not block"),
        }
    }

    #[test]
    fn text_caps_encoding_and_empty_text() {
        let text = Plan { mime: "text/plain", image: false, hint: false };
        assert!(read_selection(text, None, feed(&vec![b'a'; MAX_TEXT + 1])).is_none());
        assert!(matches!(read_selection(text, None, feed(&vec![b'a'; MAX_TEXT])), Some(Captured::Text(t)) if t.len() == MAX_TEXT));
        assert!(read_selection(text, None, feed(&[0xff, 0xfe, 0xfd])).is_none());
        let mut history = History::with_limit(5);
        assert!(history.push_text("   \n\t".into()).is_none());
        assert!(history.push_text(String::new()).is_none());
        assert!(history.push_text("x".repeat(MAX_TEXT + 1)).is_none());
        assert_eq!(history.len(), 0);
    }

    fn png_of(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, w, h);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            let data: Vec<u8> = (0..w * h).flat_map(|_| rgba).collect();
            writer.write_image_data(&data).unwrap();
        }
        out
    }

    #[test]
    fn images_are_decoded_for_a_thumbnail_and_bounded() {
        let png = png_of(300, 150, [200, 30, 30, 255]);
        let plan = Plan { mime: "image/png", image: true, hint: false };
        match read_selection(plan, None, feed(&png)).unwrap() {
            Captured::Image { width, height, thumb, png: bytes } => {
                assert_eq!((width, height), (300, 150));
                assert_eq!(bytes, png);
                let (tw, th, px) = thumb.unwrap();
                assert_eq!((tw, th), (112, 56));
                assert_eq!(&px[..4], &[200, 30, 30, 255]);
            }
            _ => panic!("expected an image"),
        }
        // Not a PNG: dropped.
        assert!(read_selection(plan, None, feed(b"not a png at all")).is_none());
        let mut history = History::with_limit(5);
        assert!(history.push_image(vec![0; MAX_IMAGE + 1], 10, 10, None).is_none());
        assert!(history.push_image(png, 300, 150, None).is_some());
    }

    #[test]
    fn history_dedupes_moves_to_front_and_keeps_the_limit() {
        let mut h = History::with_limit(3);
        let a = h.push_text("a".into()).unwrap();
        let b = h.push_text("b".into()).unwrap();
        let c = h.push_text("c".into()).unwrap();
        assert_eq!(h.entries().iter().map(|e| e.id).collect::<Vec<_>>(), [c, b, a]);
        // Copying "a" again reuses its entry at the front.
        assert_eq!(h.push_text("a".into()), Some(a));
        assert_eq!(h.entries().iter().map(|e| e.id).collect::<Vec<_>>(), [a, c, b]);
        let d = h.push_text("d".into()).unwrap();
        assert_eq!(h.len(), 3);
        assert!(h.get(b).is_none(), "oldest entry falls off");
        assert_eq!(h.entries().front().unwrap().id, d);
        h.set_limit(1);
        assert_eq!(h.len(), 1);
        assert!(h.remove(d));
        assert!(!h.remove(d));
        // Identical images are one entry too.
        let png = png_of(4, 4, [1, 2, 3, 255]);
        let i = h.push_image(png.clone(), 4, 4, None).unwrap();
        assert_eq!(h.push_image(png, 4, 4, None), Some(i));
        assert_eq!(h.len(), 1);
        h.clear();
        assert_eq!(h.len(), 0);
        h.push_text("x".into());
        h.promote(h.entries().front().unwrap().id);
    }

    #[test]
    fn previews_are_single_line_with_details() {
        let mut h = History::with_limit(5);
        let id = h.push_text("fn main() {\n    println!(\"hi\");\n}\n".into()).unwrap();
        let e = h.get(id).unwrap();
        assert_eq!(e.preview, "fn main() { println!(\"hi\"); }");
        assert_eq!(e.detail, "3 lines, 34 characters");
        assert_eq!(one_line("  a \n\n b\tc  ", 100), "a b c");
        assert_eq!(one_line("abcdefghij", 4), "abcd");
        assert_eq!(fit(2880, 1620, 112, 64), (112, 63));
        assert_eq!(fit(50, 20, 112, 64), (50, 20));
    }

    #[test]
    fn downscale_averages() {
        let mut px = Vec::new();
        for _ in 0..4 {
            px.extend_from_slice(&[10, 20, 30, 255]);
        }
        assert_eq!(downscale(&px, 2, 2, 1, 1), [10, 20, 30, 255]);
        assert_eq!(downscale(&px, 2, 2, 2, 2), px);
    }
}
