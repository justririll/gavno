//! On-demand music browser. All session resources die together on close.
use crate::{module::Feature, topbar::geometry::Rect, Gavno};
use serde::{Deserialize, Serialize};
use smithay::{
    input::keyboard::{keysyms, Keysym, ModifiersState},
    output::Output,
    reexports::calloop::{
        channel,
        timer::{TimeoutAction, Timer},
        LoopHandle, RegistrationToken,
    },
    utils::{Logical, Point},
};
use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
#[path = "music_tests.rs"]
mod tests;
#[path = "music_ui.rs"]
mod ui;
#[path = "music_worker.rs"]
pub(super) mod worker;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Item {
    pub uri: String,
    pub kind: String,
    pub title: String,
    pub subtitle: String,
    pub duration_ms: u32,
    pub cover_url: Option<String>,
    pub cover_path: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    pub logged_in: bool,
    pub login_pending: bool,
    pub busy: bool,
    pub playing: bool,
    pub item: Option<Item>,
    pub position_ms: u32,
    pub position_at_ms: u64,
    pub volume: f64,
    pub message: String,
    pub upcoming: Vec<Item>,
}
impl State {
    pub fn position(&self) -> u32 {
        let elapsed = if self.playing {
            epoch_ms().saturating_sub(self.position_at_ms)
        } else {
            0
        };
        (u64::from(self.position_ms).saturating_add(elapsed)).min(u64::from(
            self.item.as_ref().map_or(u32::MAX, |i| i.duration_ms),
        )) as u32
    }
}
fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Page {
    pub title: String,
    pub description: String,
    pub items: Vec<Item>,
    pub next: Option<String>,
    pub play_uri: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Panel,
    Back,
    Tab(usize),
    Row(usize),
    More,
    Login,
    Control(&'static str),
    Seek,
    Volume,
}

#[derive(Default)]
pub struct Panel {
    pub session: Option<Session>,
    serial: u64,
    /// Private bus override used by integration tests; never changes process environment.
    pub(crate) bus_address: Option<String>,
}
pub struct Session {
    pub output: Output,
    pub state: State,
    pub page: Page,
    tabs: [Option<Page>; 3],
    pub tab: usize,
    back: Vec<HistoryEntry>,
    searching: bool,
    pub query: String,
    pub error: String,
    pub selected: usize,
    pub scroll: usize,
    pub dirty: bool,
    pub view: ui::View,
    worker: async_channel::Sender<worker::Request>,
    source: Option<RegistrationToken>,
    debounce: Option<RegistrationToken>,
    tick: Option<RegistrationToken>,
    generation: u64,
    navigation: u64,
    browse_id: u64,
    loading: bool,
    drag: Option<(Rect, Hit)>,
}
struct HistoryEntry {
    page: Page,
    query: String,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.worker.close();
    }
}
impl Session {
    pub fn send(&self, command: worker::Command) {
        let _ = self.worker.try_send(worker::Request::Command(command));
    }
    fn adjust(&mut self, area: Rect, point: Point<f64, Logical>, hit: Hit) {
        let fraction = ((point.x - area.loc.x as f64) / area.size.w.max(1) as f64).clamp(0.0, 1.0);
        if hit == Hit::Seek {
            let position =
                (self.state.item.as_ref().map_or(0, |i| i.duration_ms) as f64 * fraction) as u32;
            self.send(worker::Command::Seek(position));
            self.state.position_ms = position;
            self.state.position_at_ms = epoch_ms();
        } else {
            let volume = (fraction * 100.0).round() / 100.0;
            if (self.state.volume - volume).abs() < 0.001 {
                return;
            }
            self.send(worker::Command::Volume(volume));
            self.state.volume = volume;
        }
        self.dirty = true;
    }
    fn browse(&mut self, view: &str, arg: String) {
        self.browse_id = self.browse_id.wrapping_add(1);
        self.loading = true;
        self.error.clear();
        if self
            .worker
            .try_send(worker::Request::Browse(self.browse_id, view.into(), arg))
            .is_err()
        {
            self.loading = false;
            self.error = "Music daemon unavailable".into();
        }
        self.dirty = true;
    }
    fn invalidate_browse(&mut self) {
        self.browse_id = self.browse_id.wrapping_add(1);
        self.loading = false;
    }
    fn cancel_search(&mut self, handle: &LoopHandle<'_, Gavno>) {
        self.navigation = self.navigation.wrapping_add(1);
        if let Some(token) = self.debounce.take() {
            handle.remove(token);
        }
    }
    fn clear_account(&mut self, handle: &LoopHandle<'_, Gavno>) {
        self.cancel_search(handle);
        self.invalidate_browse();
        self.page = Page::default();
        self.tabs = Default::default();
        self.back.clear();
        self.searching = false;
        self.query.clear();
        self.error.clear();
        self.scroll = 0;
        self.selected = 0;
        self.drag = None;
        self.view = Default::default();
    }
    fn tab(&mut self, tab: usize, handle: &LoopHandle<'_, Gavno>) {
        self.cancel_search(handle);
        self.invalidate_browse();
        self.tab = tab;
        self.query.clear();
        self.back.clear();
        self.searching = false;
        self.scroll = 0;
        self.selected = 0;
        self.page = self.tabs[tab].clone().unwrap_or_default();
        if self.tabs[tab].is_none() {
            self.browse(["home", "library", "playlists"][tab], String::new());
        }
        self.dirty = true;
    }
    fn back(&mut self, handle: &LoopHandle<'_, Gavno>) {
        self.cancel_search(handle);
        if let Some(entry) = self.back.pop() {
            self.invalidate_browse();
            self.page = entry.page;
            self.query = entry.query;
            self.searching = !self.query.is_empty();
            self.scroll = 0;
            self.selected = 0;
            self.dirty = true;
        }
    }
    fn activate(&mut self, index: usize, handle: &LoopHandle<'_, Gavno>) {
        let Some(item) = self.page.items.get(index) else {
            if index == self.page.items.len() && !self.loading {
                if let Some(next) = self.page.next.clone() {
                    self.browse("more", next);
                }
            }
            return;
        };
        match item.kind.as_str() {
            "track" | "episode" => self.send(worker::Command::Play(
                item.uri.clone(),
                self.page.play_uri.clone().unwrap_or_default(),
            )),
            "album" | "playlist" | "artist" | "show" => {
                let uri = item.uri.clone();
                self.cancel_search(handle);
                self.back.push(HistoryEntry {
                    page: self.page.clone(),
                    query: std::mem::take(&mut self.query),
                });
                self.searching = false;
                self.scroll = 0;
                self.selected = 0;
                self.browse("entity", uri);
            }
            _ => (),
        }
    }
    fn cover_ready(&mut self, url: &str, path: &str) {
        fn update(item: &mut Item, url: &str, path: &str) {
            if item.cover_url.as_deref() == Some(url) {
                item.cover_path = Some(path.to_owned());
            }
        }
        for page in std::iter::once(&mut self.page)
            .chain(self.tabs.iter_mut().flatten())
            .chain(self.back.iter_mut().map(|entry| &mut entry.page))
        {
            for item in &mut page.items {
                update(item, url, path);
            }
        }
        if let Some(item) = &mut self.state.item {
            update(item, url, path);
        }
        for item in &mut self.state.upcoming {
            update(item, url, path);
        }
    }
}

impl Gavno {
    pub fn music_open(&self) -> bool {
        self.features.on(Feature::Media) && self.now_playing.borrow().music.session.is_some()
    }
    pub fn toggle_music(&mut self, output: &Output) {
        if self.music_open() {
            self.close_music();
            return;
        }
        if !self.config.media.music_panel
            || self.protocols.locks.locked
            || self.lock.internal()
            || self.greeter.is_some()
        {
            return;
        }
        self.guard(Feature::Media, (), |gv| {
            gv.close_launcher();
            gv.topbar.close();
            let (tx, rx) = channel::channel();
            let mut media = gv.now_playing.borrow_mut();
            media.music.serial = media.music.serial.wrapping_add(1);
            let generation = media.music.serial;
            let worker = worker::spawn(tx, media.music.bus_address.clone());
            media.music.session = Some(Session {
                output: output.clone(),
                state: State::default(),
                page: Page::default(),
                tabs: Default::default(),
                tab: 0,
                back: Vec::new(),
                searching: false,
                query: String::new(),
                error: String::new(),
                selected: 0,
                scroll: 0,
                dirty: true,
                view: Default::default(),
                worker,
                source: None,
                debounce: None,
                tick: None,
                generation,
                navigation: 0,
                browse_id: 0,
                loading: false,
                drag: None,
            });
            drop(media);
            match gv.loop_handle.insert_source(rx, move |event, _, gv| {
                if let channel::Event::Msg(event) = event {
                    gv.guard(Feature::Media, (), |gv| gv.music_event(generation, event));
                }
            }) {
                Ok(token) => {
                    gv.now_playing
                        .borrow_mut()
                        .music
                        .session
                        .as_mut()
                        .unwrap()
                        .source = Some(token);
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot register music channel");
                    gv.close_music();
                    return;
                }
            }
            gv.now_playing
                .borrow_mut()
                .music
                .session
                .as_mut()
                .unwrap()
                .tab(0, &gv.loop_handle);
            gv.cancel_shell_tap();
            gv.queue_redraw_all();
        });
    }
    pub fn close_music(&mut self) {
        let session = self.now_playing.borrow_mut().music.session.take();
        if let Some(mut session) = session {
            session.worker.close();
            for token in [
                session.source.take(),
                session.debounce.take(),
                session.tick.take(),
            ]
            .into_iter()
            .flatten()
            {
                self.loop_handle.remove(token);
            }
            drop(session);
            crate::release_free_heap();
            self.queue_redraw_all();
        }
    }
    pub(crate) fn refresh_music_gate(&mut self) {
        let gone = self
            .now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .is_some_and(|s| self.space.output_geometry(&s.output).is_none());
        if gone
            || self.protocols.locks.locked
            || self.lock.internal()
            || self.greeter.is_some()
            || !self.config.media.music_panel
        {
            self.close_music();
        }
    }
    fn music_event(&mut self, generation: u64, event: worker::Event) {
        let mut media = self.now_playing.borrow_mut();
        let Some(s) = media
            .music
            .session
            .as_mut()
            .filter(|s| s.generation == generation)
        else {
            return;
        };
        match event {
            worker::Event::State(state) => {
                let logged_in = state.logged_in && !s.state.logged_in;
                if s.state.logged_in && !state.logged_in {
                    s.clear_account(&self.loop_handle);
                }
                s.state = state;
                if logged_in && s.tabs[s.tab].is_none() && !s.loading {
                    s.browse(["home", "library", "playlists"][s.tab], String::new());
                }
            }
            worker::Event::Page(id, more, result) if id == s.browse_id => {
                s.loading = false;
                match result {
                    Ok(mut page) => {
                        if more {
                            s.page.items.append(&mut page.items);
                            s.page.next = page.next;
                        } else {
                            s.page = page;
                            s.selected = 0;
                            s.scroll = 0;
                        }
                        if s.query.is_empty() && s.back.is_empty() {
                            s.tabs[s.tab] = Some(s.page.clone());
                        }
                    }
                    Err(error) => s.error = error,
                }
            }
            worker::Event::Page(..) => return,
            worker::Event::Unavailable => {
                s.clear_account(&self.loop_handle);
                s.state = State::default();
                s.error = "Music daemon unavailable".into();
            }
            worker::Event::Cover(url, path, image) => {
                s.cover_ready(&url, &path);
                if let Some(image) = image {
                    s.view.covers.insert(path, image);
                }
            }
            worker::Event::Error(error) => {
                s.loading = false;
                s.error = error;
            }
        }
        s.dirty = true;
        let playing = s.state.playing;
        if !playing {
            if let Some(token) = s.tick.take() {
                self.loop_handle.remove(token);
            }
        }
        let needs_tick = playing && s.tick.is_none();
        drop(media);
        if needs_tick {
            self.music_tick();
        }
        self.queue_redraw_all();
    }
    fn music_tick(&mut self) {
        let token = self
            .loop_handle
            .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, gv| {
                gv.guard(Feature::Media, (), |gv| {
                    let mut media = gv.now_playing.borrow_mut();
                    let Some(s) = media.music.session.as_mut() else {
                        return;
                    };
                    s.tick = None;
                    if !s.state.playing {
                        return;
                    }
                    s.dirty = true;
                    drop(media);
                    gv.queue_redraw_all();
                    gv.music_tick();
                });
                TimeoutAction::Drop
            })
            .ok();
        if let Some(s) = self.now_playing.borrow_mut().music.session.as_mut() {
            s.tick = token;
        }
    }
    fn music_debounce(&mut self) {
        let mut media = self.now_playing.borrow_mut();
        let Some(s) = media.music.session.as_mut() else {
            return;
        };
        s.cancel_search(&self.loop_handle);
        s.invalidate_browse();
        if s.query.is_empty() {
            if s.searching {
                s.back(&self.loop_handle);
            }
            return;
        }
        if !s.searching {
            s.back.push(HistoryEntry {
                page: s.page.clone(),
                query: String::new(),
            });
            s.searching = true;
        }
        let generation = s.generation;
        let navigation = s.navigation;
        s.debounce = self
            .loop_handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(300)),
                move |_, _, gv| {
                    gv.guard(Feature::Media, (), |gv| {
                        gv.music_search(generation, navigation)
                    });
                    TimeoutAction::Drop
                },
            )
            .ok();
    }
    fn music_search(&mut self, generation: u64, navigation: u64) {
        if let Some(s) = self
            .now_playing
            .borrow_mut()
            .music
            .session
            .as_mut()
            .filter(|s| s.generation == generation && s.navigation == navigation && s.searching)
        {
            s.debounce = None;
            s.browse("search", s.query.clone());
        }
        self.queue_redraw_all();
    }
    pub fn music_key(&mut self, sym: Keysym, ch: Option<char>, mods: &ModifiersState) {
        self.guard(Feature::Media, (), |gv| {
            if sym.raw() == keysyms::KEY_Escape {
                gv.close_music();
                return;
            }
            let mut media = gv.now_playing.borrow_mut();
            let Some(s) = media.music.session.as_mut() else {
                return;
            };
            let mut edited = false;
            match sym.raw() {
                keysyms::KEY_1..=keysyms::KEY_3 if mods.ctrl => {
                    s.tab((sym.raw() - keysyms::KEY_1) as usize, &gv.loop_handle);
                }
                keysyms::KEY_Left if mods.alt => {
                    s.back(&gv.loop_handle);
                }
                keysyms::KEY_BackSpace if s.query.is_empty() => s.back(&gv.loop_handle),
                keysyms::KEY_BackSpace => {
                    s.query.pop();
                    edited = true;
                }
                keysyms::KEY_Up | keysyms::KEY_Down => {
                    let step: isize = if sym.raw() == keysyms::KEY_Up { -1 } else { 1 };
                    let len = s.page.items.len() + usize::from(s.page.next.is_some());
                    let mut index = s.selected as isize + step;
                    while index >= 0
                        && (index as usize) < len
                        && s.page
                            .items
                            .get(index as usize)
                            .is_some_and(|i| i.kind == "section")
                    {
                        index += step;
                    }
                    if index >= 0 && (index as usize) < len {
                        s.selected = index as usize;
                    }
                    let visible = s.view.visible_rows.max(1);
                    if s.selected < s.scroll {
                        s.scroll = s.selected;
                    }
                    if s.selected >= s.scroll + visible {
                        s.scroll = s.selected + 1 - visible;
                    }
                }
                keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                    s.activate(s.selected, &gv.loop_handle)
                }
                _ => {
                    if let Some(c) = ch.filter(|c| !c.is_control() && !mods.ctrl && !mods.alt) {
                        if s.query.len() < 1024 {
                            s.query.push(c);
                            edited = true;
                        }
                    }
                }
            }
            s.dirty = true;
            drop(media);
            if edited {
                gv.music_debounce();
            }
            gv.queue_redraw_all();
        });
    }
    pub fn music_button(&mut self, point: Point<f64, Logical>, pressed: bool) -> bool {
        if !self.music_open() {
            return false;
        }
        self.guard(Feature::Media, true, |gv| {
            if !pressed {
                gv.music_release();
                return true;
            }
            let mut media = gv.now_playing.borrow_mut();
            let s = media.music.session.as_mut().unwrap();
            let local = gv
                .space
                .output_geometry(&s.output)
                .map(|g| point - g.loc.to_f64());
            let hit = local.and_then(|p| s.view.hit(p));
            match hit {
                None => {
                    drop(media);
                    gv.close_music();
                    return true;
                }
                Some((_, Hit::Back)) => {
                    s.back(&gv.loop_handle);
                }
                Some((_, Hit::Tab(tab))) => {
                    s.tab(tab, &gv.loop_handle);
                }
                Some((_, Hit::Row(index))) => s.activate(index, &gv.loop_handle),
                Some((_, Hit::More)) => {
                    if !s.loading {
                        if let Some(next) = s.page.next.clone() {
                            s.browse("more", next);
                        }
                    }
                }
                Some((_, Hit::Login)) => s.send(worker::Command::Login),
                Some((_, Hit::Control(command))) => {
                    s.send(worker::Command::Control(command.into()))
                }
                Some((area, hit @ (Hit::Seek | Hit::Volume))) => {
                    s.drag = Some((area, hit));
                    s.adjust(area, local.unwrap(), hit);
                }
                _ => (),
            }
            s.dirty = true;
            drop(media);
            gv.queue_redraw_all();
            true
        })
    }
    pub fn music_release(&mut self) {
        if let Some(s) = self.now_playing.borrow_mut().music.session.as_mut() {
            s.drag = None;
        }
    }
    pub fn music_motion(&mut self, point: Point<f64, Logical>) -> bool {
        if !self.music_open() {
            return false;
        }
        self.guard(Feature::Media, true, |gv| {
            if gv.polkit_visible() {
                return true;
            }
            let mut media = gv.now_playing.borrow_mut();
            let s = media.music.session.as_mut().unwrap();
            if let Some((area, hit)) = s.drag {
                if let Some(geometry) = gv.space.output_geometry(&s.output) {
                    s.adjust(area, point - geometry.loc.to_f64(), hit);
                }
            }
            true
        })
    }
    pub fn music_axis(&mut self, vertical: f64) -> bool {
        if !self.music_open() {
            return false;
        }
        self.guard(Feature::Media, true, |gv| {
            if vertical != 0.0 {
                if let Some(s) = gv.now_playing.borrow_mut().music.session.as_mut() {
                    let max = (s.page.items.len() + usize::from(s.page.next.is_some()))
                        .saturating_sub(s.view.visible_rows.max(1));
                    s.scroll = s
                        .scroll
                        .saturating_add_signed(if vertical > 0.0 { 1 } else { -1 })
                        .min(max);
                    s.dirty = true;
                }
                gv.queue_redraw_all();
            }
            true
        })
    }
}
// Keep these imports local to this module's public UI vocabulary.
pub(super) type Covers =
    HashMap<String, smithay::backend::renderer::element::memory::MemoryRenderBuffer>;
pub(super) type Hits = Vec<(Rect, Hit)>;
