//! One lazy worker multiplexes requests and signals without polling or a signal thread.
use super::{Item, Page, State};
use futures_lite::{future, StreamExt};
use smithay::{
    backend::renderer::element::memory::MemoryRenderBuffer, reexports::calloop::channel,
};
use std::{
    collections::{HashMap, HashSet},
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
}
pub enum Event {
    State(State),
    Page(u64, bool, Result<Page, String>),
    Cover(String, String, Option<MemoryRenderBuffer>),
    Error(String),
}
pub fn spawn(
    events: channel::Sender<Event>,
    address: Option<String>,
) -> async_channel::Sender<Request> {
    let (tx, rx) = async_channel::unbounded();
    let failed = events.clone();
    let stop = tx.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("music-panel".into())
        .spawn(move || {
            let result = (|| {
                let builder = match address.as_deref() {
                    Some(address) => zbus::blocking::connection::Builder::address(address)?,
                    None => zbus::blocking::connection::Builder::session()?,
                };
                let connection = builder.method_timeout(Duration::from_secs(15)).build()?;
                run(connection, events.clone(), rx, stop)
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
struct Images {
    decoded: HashSet<String>,
    ready: HashMap<String, String>,
}
impl Images {
    fn decode(&mut self, events: &channel::Sender<Event>, url: String, path: String) {
        if !self.decoded.contains(&path) {
            let image = crate::icons::music_cover(&path);
            if image.is_some() {
                self.decoded.insert(path.clone());
            }
            let _ = events.send(Event::Cover(url, path, image));
        } else {
            let _ = events.send(Event::Cover(url, path, None));
        }
    }
    fn items<'a>(
        &mut self,
        events: &channel::Sender<Event>,
        items: impl Iterator<Item = &'a mut Item>,
    ) {
        for item in items {
            if item.cover_path.is_none() {
                item.cover_path = item
                    .cover_url
                    .as_ref()
                    .and_then(|u| self.ready.get(u))
                    .cloned();
            }
            if let Some(path) = &item.cover_path {
                self.decode(
                    events,
                    item.cover_url.clone().unwrap_or_default(),
                    path.clone(),
                );
            }
        }
    }
    fn state(&mut self, events: &channel::Sender<Event>, json: &str) -> zbus::Result<()> {
        let mut state: State =
            serde_json::from_str(json).map_err(|e| zbus::Error::Failure(e.to_string()))?;
        self.items(
            events,
            state.item.iter_mut().chain(state.upcoming.iter_mut()),
        );
        let _ = events.send(Event::State(state));
        Ok(())
    }
}
fn run(
    connection: Connection,
    events: channel::Sender<Event>,
    rx: async_channel::Receiver<Request>,
    stop: async_channel::Sender<Request>,
) -> zbus::Result<()> {
    let rule = format!("type='signal',sender='{NAME}',path='{PATH}',interface='{IFACE}'");
    let mut signals =
        MessageIterator::for_match_rule(rule.as_str(), &connection, Some(64))?.into_inner();
    let proxy = Proxy::new(&connection, NAME, PATH, IFACE)?;
    let mut images = Images {
        decoded: HashSet::new(),
        ready: HashMap::new(),
    };
    match call(&stop, proxy.inner().call::<_, _, String>("State", &())) {
        Ok(json) => images.state(&events, &json)?,
        Err(error) => {
            let _ = events.send(Event::Error(error.to_string()));
        }
    }
    type BrowseReply = (u64, bool, zbus::Result<String>);
    type PendingBrowse<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = BrowseReply> + 'a>>;
    let mut pending: Option<PendingBrowse<'_>> = None;
    enum Wake {
        Request(Result<Request, async_channel::RecvError>),
        Signal(Option<zbus::Result<zbus::Message>>),
        Page(BrowseReply),
    }
    while !rx.is_closed() {
        let wake = future::block_on(future::or(
            async { Wake::Request(rx.recv().await) },
            future::or(async { Wake::Signal(signals.next().await) }, async {
                match pending.as_mut() {
                    Some(task) => Wake::Page(task.await),
                    None => std::future::pending().await,
                }
            }),
        ));
        if rx.is_closed() {
            break;
        }
        match wake {
            Wake::Request(Err(_)) | Wake::Signal(None) => break,
            Wake::Request(Ok(Request::Browse(id, view, arg))) => {
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
                    images.items(&events, page.items.iter_mut());
                    page
                });
                if events.send(Event::Page(id, more, page)).is_err() {
                    break;
                }
            }
            Wake::Request(Ok(Request::Command(command))) => {
                let result: zbus::Result<()> = match command {
                    Command::Play(uri, context) => {
                        call(&stop, proxy.inner().call("Play", &(uri, context)))
                    }
                    Command::Control(command) => {
                        call(&stop, proxy.inner().call("Control", &(command,)))
                    }
                    Command::Seek(position) => {
                        call(&stop, proxy.inner().call("Seek", &(position,)))
                    }
                    Command::Volume(volume) => {
                        call(&stop, proxy.inner().call("SetVolume", &(volume,)))
                    }
                    Command::Login => call(&stop, proxy.inner().call("Login", &())),
                };
                if let Err(error) = result {
                    let _ = events.send(Event::Error(error.to_string()));
                }
            }
            Wake::Signal(Some(Ok(message))) => {
                match message.header().member().map(|m| m.as_str()) {
                    Some("StateChanged") => {
                        if let Ok(json) = message.body().deserialize::<String>() {
                            images.state(&events, &json)?;
                        }
                    }
                    Some("CoverReady") => {
                        if let Ok((url, path)) = message.body().deserialize::<(String, String)>() {
                            images.ready.insert(url.clone(), path.clone());
                            images.decode(&events, url, path);
                        }
                    }
                    _ => (),
                }
            }
            Wake::Signal(Some(Err(error))) => {
                let _ = events.send(Event::Error(error.to_string()));
                break;
            }
        }
    }
    drop(pending);
    drop(proxy);
    drop(signals);
    connection.close()
}

/// Cancel an outstanding D-Bus call immediately when the panel closes.
fn call<T>(
    stop: &async_channel::Sender<Request>,
    task: impl std::future::Future<Output = zbus::Result<T>>,
) -> zbus::Result<T> {
    future::block_on(future::or(task, async {
        stop.closed().await;
        Err(zbus::Error::Failure("Music panel closed".into()))
    }))
}
