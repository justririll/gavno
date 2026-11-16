//! Event-driven MPRIS state shared by the bar and the built-in lock UI.
pub(crate) mod mpris;
pub(crate) mod ui;

use crate::{module::Feature, Gavno};
use smithay::{
    backend::renderer::element::memory::MemoryRenderBuffer, reexports::calloop::channel,
};
use std::{cell::RefCell, rc::Rc, sync::mpsc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    PlayPause,
    Next,
    Previous,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Playback {
    Playing,
    Paused,
    #[default]
    Stopped,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub title: String,
    pub artists: Vec<String>,
    pub art_url: String,
    pub length: Option<i64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Player {
    pub name: String,
    pub identity: String,
    pub desktop: String,
    pub playback: Playback,
    pub metadata: Metadata,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_play: bool,
    pub can_pause: bool,
    pub started: u64,
    pub updated: u64,
}
impl Player {
    pub fn title(&self) -> &str {
        if self.metadata.title.is_empty() {
            &self.identity
        } else {
            &self.metadata.title
        }
    }
    pub fn artist(&self) -> String {
        self.metadata.artists.join(", ")
    }
    pub fn label(&self) -> String {
        let artist = self.artist();
        if artist.is_empty() {
            self.title().to_owned()
        } else {
            format!("{} — {artist}", self.title())
        }
    }
    pub fn allows(&self, command: Command) -> bool {
        match command {
            Command::Next => self.can_next,
            Command::Previous => self.can_previous,
            Command::PlayPause => {
                if self.playback == Playback::Playing {
                    self.can_pause
                } else {
                    self.can_play
                }
            }
        }
    }
}
/// Playing always wins; updates to a playing track do not change its start order.
pub fn active_player(players: &[Player]) -> Option<&Player> {
    players
        .iter()
        .filter(|p| p.playback == Playback::Playing)
        .max_by_key(|p| (p.started, &p.name))
        .or_else(|| {
            players
                .iter()
                .filter(|p| p.playback == Playback::Paused)
                .max_by_key(|p| (p.updated, &p.name))
        })
}
#[derive(Clone, Default)]
pub struct Snapshot {
    pub player: Option<Player>,
    pub image: Option<MemoryRenderBuffer>,
}
#[derive(Default)]
pub struct Media {
    pub snapshot: Snapshot,
    pub worker: Option<mpsc::Sender<mpris::Request>>,
    started: bool,
}
pub type Shared = Rc<RefCell<Media>>;

impl Gavno {
    pub fn refresh_media(&mut self) {
        self.guard(Feature::Media, (), |gv| {
            if gv.now_playing.borrow().started
                || gv.greeter.is_some()
                || matches!(gv.backend, crate::backend::Backend::None)
            {
                return;
            }
            gv.now_playing.borrow_mut().started = true;
            let _startup = crate::perf::Startup::new("dbus.media.spawn.begin", "dbus.media.spawn.end");
            if let Some(tx) = gv.media_events() {
                gv.now_playing.borrow_mut().worker =
                    mpris::spawn(tx, gv.config.dock.icon_theme.clone());
            }
        });
    }
    pub(crate) fn media_events(&self) -> Option<channel::Sender<Snapshot>> {
        let (tx, rx) = channel::channel();
        let generation=self.media_generation;
        match self.loop_handle.insert_source(rx, move |event, _, gv| {
            let _stall = crate::perf::Stall::new("dbus.media");
            if let channel::Event::Msg(snapshot) = event {
                if gv.media_generation==generation {gv.media_update(snapshot);}
            }
        }) {
            Ok(_) => Some(tx),
            Err(error) => {
                tracing::warn!(%error, "cannot register media channel");
                None
            }
        }
    }
    pub(crate) fn media_update(&mut self, snapshot: Snapshot) {
        self.guard(Feature::Media, (), |gv| {
            gv.classic.desktop.widgets.update_player(snapshot.player.as_ref());
            gv.now_playing.borrow_mut().snapshot = snapshot;
            gv.queue_redraw_all();
        });
    }
    pub fn media_command(&mut self, command: Command) {
        self.guard(Feature::Media, (), |gv| {
            let media = gv.now_playing.borrow();
            if media
                .snapshot
                .player
                .as_ref()
                .is_some_and(|p| p.allows(command))
            {
                if let Some(worker) = &media.worker {
                    let _ = worker.send(mpris::Request::Command(command));
                }
            }
        });
    }
    pub(crate) fn reset_media(&mut self) {
        self.now_playing.borrow_mut().snapshot = Snapshot::default();
        self.topbar.clear_media_hits();
        self.lock.clear_media();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_prefers_latest_start_then_latest_paused_update() {
        let mut players = vec![
            Player {
                name: "a".into(),
                playback: Playback::Playing,
                started: 1,
                updated: 9,
                ..Default::default()
            },
            Player {
                name: "b".into(),
                playback: Playback::Playing,
                started: 2,
                updated: 2,
                ..Default::default()
            },
            Player {
                name: "c".into(),
                playback: Playback::Paused,
                updated: 10,
                ..Default::default()
            },
        ];
        assert_eq!(active_player(&players).unwrap().name, "b");
        players[1].playback = Playback::Paused;
        assert_eq!(active_player(&players).unwrap().name, "a");
        players[0].playback = Playback::Stopped;
        assert_eq!(active_player(&players).unwrap().name, "c");
        players.remove(2);
        assert_eq!(active_player(&players).unwrap().name, "b");
        players[1].playback = Playback::Stopped;
        assert!(active_player(&players).is_none());
    }
}
