//! One lazy worker multiplexes requests and signals without polling or a signal thread.
use super::{Item, Page, State};
use futures_lite::{future, StreamExt};
use smithay::{
    backend::renderer::element::memory::MemoryRenderBuffer, reexports::calloop::channel,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
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
    Unavailable,
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
    decoded: HashSet<String>,
    ready: HashMap<String, String>,
    logged_in: bool,
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
    fn state(&mut self, events: &channel::Sender<Event>, json: &str) -> zbus::Result<bool> {
        let mut state: State =
            serde_json::from_str(json).map_err(|e| zbus::Error::Failure(e.to_string()))?;
        let logged_out = self.logged_in && !state.logged_in;
        if logged_out {
            *self = Self::default();
        }
        self.logged_in = state.logged_in;
        self.items(
            events,
            state.item.iter_mut().chain(state.upcoming.iter_mut()),
        );
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
                    images.items(&events, page.items.iter_mut());
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
                            images.ready.insert(url.clone(), path.clone());
                            images.decode(&events, url, path);
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
