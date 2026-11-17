//! One lazy worker multiplexes requests and signals without polling or a signal thread.
use super::{Item, Page, State};
use futures_lite::{future, StreamExt};
use smithay::{
    backend::renderer::element::memory::MemoryRenderBuffer, reexports::calloop::channel,
};
use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};
use zbus::blocking::{Connection, MessageIterator, Proxy};
const NAME: &str = "org.gavno.Music";
const PATH: &str = "/org/gavno/Music";
const IFACE: &str = "org.gavno.Music1";

pub enum Command {
    Play(String, String),
    Control(String),
    Seek(u32),
    Volume(f64),
    Login,
}
pub enum Request {
    Browse(u64, String, String),
    Command(Command),
    Cover(String, CoverKind),
}
pub enum Event {
    State(State),
    Unavailable,
    Page(u64, bool, Result<Page, String>),
    CoverReady(String, String),
    Cover(String, CoverKind, Option<Cover>),
    Error(String),
}
pub fn spawn(
    events: channel::Sender<Event>,
    address: Option<String>,
) -> async_channel::Sender<Request> {
    let (tx, rx) = async_channel::unbounded();
    let failed = events.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("music-panel".into())
        .spawn(move || {
            let result = (|| {
                let builder = match address.as_deref() {
                    Some(address) => zbus::blocking::connection::Builder::address(address)?,
                    None => zbus::blocking::connection::Builder::session()?,
                };
                let connection = builder.method_timeout(Duration::from_secs(15)).build()?;
                run(connection, events.clone(), rx)
            })();
            if let Err(error) = result {
                let _ = events.send(Event::Error(format!("Music daemon unavailable: {error}")));
            }
        })
    {
        let _ = failed.send(Event::Error(format!("Cannot start music worker: {error}")));
    }
    tx
}
#[derive(Default)]
struct Images {
    ready: HashMap<String, String>,
    order: VecDeque<String>,
    logged_in: bool,
}
impl Images {
    fn items<'a>(&self, items: impl Iterator<Item = &'a mut Item>) {
        for item in items {
            if item.cover_path.is_none() {
                item.cover_path = item
                    .cover_url
                    .as_ref()
                    .and_then(|u| self.ready.get(u))
                    .cloned();
            }
        }
    }
    fn ready(&mut self, url: String, path: String) {
        if !self.ready.contains_key(&url) {
            if self.order.len() == 64 {
                if let Some(old) = self.order.pop_front() {
                    self.ready.remove(&old);
                }
            }
            self.order.push_back(url.clone());
        }
        self.ready.insert(url, path);
    }
    fn state(&mut self, events: &channel::Sender<Event>, json: &str) -> zbus::Result<bool> {
        let mut state: State =
            serde_json::from_str(json).map_err(|e| zbus::Error::Failure(e.to_string()))?;
        let logged_out = self.logged_in && !state.logged_in;
        if logged_out {
            *self = Self::default();
        }
        self.logged_in = state.logged_in;
        self.items(state.item.iter_mut().chain(state.upcoming.iter_mut()));
        let _ = events.send(Event::State(state));
        Ok(logged_out)
    }
}
fn run(
    connection: Connection,
    events: channel::Sender<Event>,
    rx: async_channel::Receiver<Request>,
) -> zbus::Result<()> {
    let rule = format!("type='signal',sender='{NAME}',path='{PATH}',interface='{IFACE}'");
    let mut signals =
        MessageIterator::for_match_rule(rule.as_str(), &connection, Some(64))?.into_inner();
    let owner_rule = format!(
        "type='signal',sender='org.freedesktop.DBus',path='/org/freedesktop/DBus',\
         interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='{NAME}'"
    );
    let mut owners =
        MessageIterator::for_match_rule(owner_rule.as_str(), &connection, Some(64))?.into_inner();
    let mut owner = zbus::blocking::fdo::DBusProxy::new(&connection)?
        .get_name_owner(NAME.try_into().expect("valid music bus name"))
        .ok()
        .map(|name| name.to_string());
    let proxy = Proxy::new(&connection, NAME, PATH, IFACE)?;
    let mut images = Images::default();
    let mut needs_state = true;
    let mut state: Option<Pending<'_, zbus::Result<String>>> =
        Some(Box::pin(proxy.inner().call("State", &())));
    type BrowseReply = (u64, bool, zbus::Result<String>);
    let mut pending: Option<Pending<'_, BrowseReply>> = None;
    let mut commands: VecDeque<Pending<'_, zbus::Result<()>>> = VecDeque::new();
    enum Wake {
        Request(Result<Request, async_channel::RecvError>),
        Signal(Option<zbus::Result<zbus::Message>>),
        Owner(Option<zbus::Result<zbus::Message>>),
        Page(BrowseReply),
        State(zbus::Result<String>),
        Command(zbus::Result<()>),
    }
    while !rx.is_closed() {
        let wake = future::block_on(future::or(
            async { Wake::Owner(owners.next().await) },
            future::or(
                async { Wake::Signal(signals.next().await) },
                future::or(
                    async { Wake::Request(rx.recv().await) },
                    future::or(
                        async { Wake::State(finish(&mut state).await) },
                        future::or(async { Wake::Page(finish(&mut pending).await) }, async {
                            match commands.front_mut() {
                                Some(task) => Wake::Command(task.await),
                                None => std::future::pending().await,
                            }
                        }),
                    ),
                ),
            ),
        ));
        if rx.is_closed() {
            break;
        }
        match wake {
            Wake::Request(Err(_)) | Wake::Signal(None) | Wake::Owner(None) => break,
            Wake::Request(Ok(Request::Cover(path, kind))) => {
                let cover = decode_cover(&path, kind);
                let _ = events.send(Event::Cover(path, kind, cover));
            }
            Wake::Request(Ok(Request::Browse(id, view, arg))) => {
                if needs_state && state.is_none() {
                    state = Some(Box::pin(proxy.inner().call("State", &())));
                }
                // New navigation replaces the old future. Playback controls and
                // signals remain responsive during a slow network-backed Browse.
                let proxy = proxy.inner();
                pending = Some(Box::pin(async move {
                    let result = proxy
                        .call::<_, _, String>("Browse", &(view.as_str(), arg.as_str()))
                        .await;
                    (id, view == "more", result)
                }));
            }
            Wake::Page((id, more, result)) => {
                pending = None;
                let page = result.map_err(|e| e.to_string()).and_then(|json| {
                    serde_json::from_str::<Page>(&json).map_err(|e| e.to_string())
                });
                let page = page.map(|mut page| {
                    images.items(page.items.iter_mut());
                    page
                });
                if events.send(Event::Page(id, more, page)).is_err() {
                    break;
                }
            }
            Wake::Request(Ok(Request::Command(command))) => {
                if needs_state && state.is_none() {
                    state = Some(Box::pin(proxy.inner().call("State", &())));
                }
                let proxy = proxy.inner();
                commands.push_back(Box::pin(async move {
                    match command {
                        Command::Play(uri, context) => proxy.call("Play", &(uri, context)).await,
                        Command::Control(command) => proxy.call("Control", &(command,)).await,
                        Command::Seek(position) => proxy.call("Seek", &(position,)).await,
                        Command::Volume(volume) => proxy.call("SetVolume", &(volume,)).await,
                        Command::Login => proxy.call("Login", &()).await,
                    }
                }));
            }
            Wake::Command(result) => {
                commands.pop_front();
                if let Err(error) = result {
                    let _ = events.send(Event::Error(error.to_string()));
                }
            }
            Wake::State(result) => {
                state = None;
                match result {
                    Ok(json) => {
                        if images.state(&events, &json)? {
                            pending = None;
                            commands.clear();
                        }
                        needs_state = false;
                    }
                    Err(error) => {
                        let _ = events.send(Event::Error(error.to_string()));
                    }
                }
            }
            Wake::Owner(Some(Ok(message))) => {
                if let Ok((_, old, new)) = message.body().deserialize::<(String, String, String)>()
                {
                    if !old.is_empty() {
                        // Dropping futures fails all in-flight work without waiting
                        // for method timeouts or retaining the previous account.
                        pending = None;
                        state = None;
                        commands.clear();
                        images = Images::default();
                        needs_state = true;
                        let _ = events.send(Event::Unavailable);
                    }
                    if !new.is_empty() {
                        state = Some(Box::pin(proxy.inner().call("State", &())));
                    }
                    owner = if new.is_empty() { None } else { Some(new) };
                }
            }
            Wake::Signal(Some(Ok(message))) => {
                // Separate signal streams can already contain messages from the
                // old owner when NameOwnerChanged is handled.
                if message.header().sender().map(|name| name.as_str()) != owner.as_deref() {
                    continue;
                }
                match message.header().member().map(|m| m.as_str()) {
                    Some("StateChanged") => {
                        if let Ok(json) = message.body().deserialize::<String>() {
                            if images.state(&events, &json)? {
                                pending = None;
                                commands.clear();
                            }
                            state = None;
                            needs_state = false;
                        }
                    }
                    Some("CoverReady") if images.logged_in => {
                        if let Ok((url, path)) = message.body().deserialize::<(String, String)>() {
                            images.ready(url.clone(), path.clone());
                            let _ = events.send(Event::CoverReady(url, path));
                        }
                    }
                    _ => (),
                }
            }
            Wake::Signal(Some(Err(error))) | Wake::Owner(Some(Err(error))) => {
                let _ = events.send(Event::Unavailable);
                let _ = events.send(Event::Error(error.to_string()));
                break;
            }
        }
    }
    drop(pending);
    drop(state);
    drop(commands);
    drop(proxy);
    drop(signals);
    drop(owners);
    connection.close()
}

type Pending<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

async fn finish<T>(task: &mut Option<Pending<'_, T>>) -> T {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

/// Decoded variants have their final alpha mask baked in on this worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CoverKind {
    List,
    Artist,
    Hero,
}
impl CoverKind {
    pub fn size(self) -> u32 {
        if self == Self::Hero {
            160
        } else {
            64
        }
    }
}
pub use crate::media::ui::Cover;
/// Decode bounded local PNGs. No file access or pixel work reaches the UI thread.
pub(crate) fn decode_cover(path: &str, kind: CoverKind) -> Option<Cover> {
    use smithay::{backend::allocator::Fourcc, utils::Transform};
    let path = std::path::Path::new(path);
    if !path.is_absolute()
        || path.extension()? != "png"
        || std::fs::metadata(path).ok()?.len() > 16 * 1024 * 1024
    {
        return None;
    }
    let mut decoder = png::Decoder::new(std::fs::File::open(path).ok()?);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }
    let mut data = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut data).ok()?;
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        _ => return None,
    };
    let size = kind.size();
    let mut pixels = vec![0; (size * size * 4) as usize];
    // Bilinear, centre-cropped square; interpolate premultiplied samples so
    // transparent source pixels cannot bleed dark fringes into the cover.
    let sample = |x: u32, y: u32| {
        let src = &data[((y * w + x) as usize) * channels..];
        let rgba = match channels {
            4 => [src[0], src[1], src[2], src[3]],
            3 => [src[0], src[1], src[2], 255],
            2 => [src[0], src[0], src[0], src[1]],
            _ => [src[0], src[0], src[0], 255],
        };
        [
            rgba[0] as f32 * rgba[3] as f32 / 255.0,
            rgba[1] as f32 * rgba[3] as f32 / 255.0,
            rgba[2] as f32 * rgba[3] as f32 / 255.0,
            rgba[3] as f32,
        ]
    };
    let side = w.min(h) as f32;
    for y in 0..size {
        for x in 0..size {
            let sx = ((w as f32 - side) / 2.0 + (x as f32 + 0.5) * side / size as f32 - 0.5)
                .clamp(0.0, (w - 1) as f32);
            let sy = ((h as f32 - side) / 2.0 + (y as f32 + 0.5) * side / size as f32 - 0.5)
                .clamp(0.0, (h - 1) as f32);
            let (ix, iy) = (sx.floor() as u32, sy.floor() as u32);
            let (fx, fy) = (sx - ix as f32, sy - iy as f32);
            let samples = [
                sample(ix, iy),
                sample((ix + 1).min(w - 1), iy),
                sample(ix, (iy + 1).min(h - 1)),
                sample((ix + 1).min(w - 1), (iy + 1).min(h - 1)),
            ];
            let dst = &mut pixels[((y * size + x) * 4) as usize..][..4];
            for c in 0..4 {
                dst[c] = ((samples[0][c] * (1.0 - fx) + samples[1][c] * fx) * (1.0 - fy)
                    + (samples[2][c] * (1.0 - fx) + samples[3][c] * fx) * fy)
                    .round() as u8;
            }
        }
    }
    let tint = cover_tint(&pixels);
    mask_cover(&mut pixels, size, kind);
    for p in pixels.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    Some(Cover {
        buffer: MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (size as i32, size as i32),
            1,
            Transform::Normal,
            None,
        ),
        tint,
    })
}
/// Premultiplied RGBA in, straight sRGB tint out.
pub(super) fn cover_tint(pixels: &[u8]) -> [f32; 3] {
    let (mut sum, mut weight, mut lightness, mut alpha_sum) = ([0.0; 3], 0.0, 0.0, 0.0);
    for p in pixels.chunks_exact(4).filter(|p| p[3] > 0) {
        let alpha = p[3] as f64 / 255.0;
        let lab = crate::ui::theme::oklab([
            p[0] as f32 / p[3] as f32,
            p[1] as f32 / p[3] as f32,
            p[2] as f32 / p[3] as f32,
        ]);
        let chroma = lab[1].hypot(lab[2]);
        let w = chroma * alpha;
        for c in 0..3 {
            sum[c] += lab[c] * w;
        }
        weight += w;
        lightness += lab[0] * alpha;
        alpha_sum += alpha;
    }
    let lab = if weight > alpha_sum * 0.0001 {
        let l = (sum[0] / weight).clamp(0.32, 0.48);
        let (a, b) = (sum[1] / weight, sum[2] / weight);
        let chroma = a.hypot(b);
        let factor = if chroma > 0.11 { 0.11 / chroma } else { 1.0 };
        [l, a * factor, b * factor]
    } else {
        [(lightness / alpha_sum.max(1.0)).clamp(0.32, 0.48), 0.0, 0.0]
    };
    let [l, a, b] = lab;
    let lp = (l + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let mp = (l - 0.1055613458 * a - 0.0638541728 * b).powi(3);
    let sp = (l - 0.0894841775 * a - 1.2914855480 * b).powi(3);
    [
        4.0767416621 * lp - 3.3077115913 * mp + 0.2309699292 * sp,
        -1.2684380046 * lp + 2.6097574011 * mp - 0.3413193965 * sp,
        -0.0041960863 * lp - 0.7034186147 * mp + 1.7076147010 * sp,
    ]
    .map(|c| {
        let c = if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        c.clamp(0.0, 1.0) as f32
    })
}
pub(super) fn mask_cover(pixels: &mut [u8], size: u32, kind: CoverKind) {
    let radius = if kind == CoverKind::Artist {
        size as f32 / 2.0
    } else {
        size as f32 * if kind == CoverKind::Hero { 0.09 } else { 0.16 }
    };
    let half = size as f32 / 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 + 0.5 - half).abs() - (half - radius);
            let dy = (y as f32 + 0.5 - half).abs() - (half - radius);
            let distance = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius;
            let coverage = (0.5 - distance).clamp(0.0, 1.0);
            let pixel = &mut pixels[((y * size + x) * 4) as usize..][..4];
            for c in pixel {
                *c = (*c as f32 * coverage).round() as u8;
            }
        }
    }
}
