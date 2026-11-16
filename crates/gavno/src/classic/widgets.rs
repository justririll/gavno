//! Widgets have one-shot sources only while the desktop can be seen.
use super::{cache::Layer, ShellMode, TASKBAR_HEIGHT};
use crate::{
    module::Feature,
    topbar::{
        geometry::{rect, Rect},
        status::Snapshot,
    },
    Gavno,
};
use smithay::{
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    utils::{Logical, Size},
};
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cpu {
    pub total: u64,
    pub idle: u64,
}
pub fn parse_cpu(text: &str) -> Option<Cpu> {
    let mut words = text.lines().next()?.split_whitespace();
    if words.next() != Some("cpu") {
        return None;
    }
    let values: Vec<u64> = words
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() < 4 {
        return None;
    }
    Some(Cpu {
        total: values.iter().try_fold(0u64, |a, v| a.checked_add(*v))?,
        idle: values[3].checked_add(*values.get(4).unwrap_or(&0))?,
    })
}
pub fn cpu_percent(previous: Cpu, current: Cpu) -> Option<u8> {
    let total = current.total.checked_sub(previous.total)?;
    let idle = current.idle.checked_sub(previous.idle)?;
    if total == 0 {
        return None;
    }
    Some((100u64.saturating_mul(total.saturating_sub(idle)) / total).min(100) as u8)
}
pub fn parse_memory(text: &str) -> Option<(u64, u64)> {
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            (k == key)
                .then(|| v.split_whitespace().next()?.parse::<u64>().ok())
                .flatten()
        })
    };
    let total = value("MemTotal")?;
    let available = value("MemAvailable").or_else(|| {
        Some(
            value("MemFree")?
                .saturating_add(value("Buffers").unwrap_or(0))
                .saturating_add(value("Cached").unwrap_or(0)),
        )
    })?;
    Some((total.saturating_sub(available), total))
}
#[derive(Debug, Clone, Default)]
pub struct Sample {
    pub cpu: Option<Cpu>,
    pub memory: Option<(u64, u64)>,
}
pub fn sample() -> Sample {
    Sample {
        cpu: std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|s| parse_cpu(&s)),
        memory: std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| parse_memory(&s)),
    }
}
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    pub classic: bool,
    pub visible: bool,
    pub locked: bool,
}
impl Gate {
    pub fn active(self) -> bool {
        self.classic && self.visible && !self.locked
    }
}
#[derive(Default)]
pub struct Widgets {
    pub enabled: Vec<String>,
    pub clock_timer: Option<RegistrationToken>,
    pub system_timer: Option<RegistrationToken>,
    pub sampling: bool,
    previous: Option<Cpu>,
    pub cpu: Option<u8>,
    pub memory: Option<(u64, u64)>,
    status: Snapshot,
    clock: String,
    date: String,
    pub revision: u64,
    pub refreshing: bool,
    player: Option<crate::media::Player>,
    music: Option<crate::media::Player>,
    music_image: Option<smithay::backend::renderer::element::memory::MemoryRenderBuffer>,
    music_available: bool,
}
const MEDIA_COMMANDS: [crate::media::Command; 3] = [
    crate::media::Command::Previous,
    crate::media::Command::PlayPause,
    crate::media::Command::Next,
];
/// Previous / play-pause / next buttons inside a media widget panel.
fn media_buttons(r: Rect) -> impl Iterator<Item = (crate::media::Command, Rect)> {
    MEDIA_COMMANDS
        .into_iter()
        .enumerate()
        .map(move |(i, c)| (c, rect(r.loc.x + 16 + i as i32 * 52, r.loc.y + 84, 44, 30)))
}
/// The media command under `p` when `r` is the media widget's panel.
pub fn media_button(r: Rect, p: smithay::utils::Point<f64, Logical>) -> Option<crate::media::Command> {
    media_buttons(r).find(|(_, b)| b.to_f64().contains(p)).map(|(c, _)| c)
}
impl Widgets {
    pub fn configure(&mut self, names: &[String]) {
        self.enabled = names
            .iter()
            .filter(|name| matches!(name.as_str(), "clock" | "system" | "media" | "music"))
            .fold(Vec::new(), |mut out, name| {
                if !out.contains(name) {
                    out.push(name.clone());
                }
                out
            });
        self.enabled.truncate(4);
        self.update_clock();
    }
    pub fn update_clock(&mut self) {
        let t = crate::ui::theme::LocalTime::now();
        self.clock = format!("{:02}:{:02}", t.hour, t.minute);
        self.date = format!(
            "{}, {} {} {}",
            t.weekday_name(),
            t.day,
            t.month_name(),
            t.year
        );
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn update_status(&mut self, status: &Snapshot) {
        if &self.status != status {
            self.status = status.clone();
            self.revision = self.revision.wrapping_add(1);
        }
    }
    pub fn update_player(&mut self, player: Option<&crate::media::Player>) {
        if self.player.as_ref() != player {
            self.player = player.cloned();
            self.revision = self.revision.wrapping_add(1);
        }
    }
    fn update_music(&mut self, media: &crate::media::Media) {
        let session = media.music.session.as_ref().filter(|s| s.has_state);
        let (player, image) = if let Some(session) = session {
            let player = session
                .state
                .item
                .as_ref()
                .map(|item| crate::media::Player {
                    name: crate::media::MUSIC_PLAYER.into(),
                    identity: "Spotify".into(),
                    metadata: crate::media::Metadata {
                        title: item.title.clone(),
                        artists: vec![item.subtitle.clone()],
                        art_url: item.cover_path.clone().unwrap_or_default(),
                        ..Default::default()
                    },
                    playback: if session.state.playing {
                        crate::media::Playback::Playing
                    } else {
                        crate::media::Playback::Paused
                    },
                    can_previous: true,
                    can_play: true,
                    can_pause: true,
                    can_next: true,
                    ..Default::default()
                });
            let image = session
                .state
                .item
                .as_ref()
                .and_then(|i| i.cover_path.as_ref())
                .and_then(|path| session.view.covers.get(path))
                .cloned();
            (player, image)
        } else {
            (
                media.snapshot.music_player.clone(),
                media.snapshot.music_image.clone(),
            )
        };
        let available = media.snapshot.music_available || session.is_some();
        if self.music != player
            || self.music_image.is_some() != image.is_some()
            || self.music_available != available
        {
            self.music = player;
            self.music_image = image;
            self.music_available = available;
            self.revision = self.revision.wrapping_add(1);
        }
    }
    pub fn receive(&mut self, s: Sample) {
        self.sampling = false;
        let cpu = self
            .previous
            .zip(s.cpu)
            .and_then(|(a, b)| cpu_percent(a, b));
        if s.cpu.is_some() {
            self.previous = s.cpu;
        }
        if self.cpu != cpu || self.memory != s.memory {
            self.cpu = cpu;
            self.memory = s.memory;
            self.revision = self.revision.wrapping_add(1);
        }
    }
    pub fn layouts<'a>(
        &'a self,
        positions: &'a BTreeMap<String, [i32; 2]>,
        size: Size<i32, Logical>,
    ) -> impl Iterator<Item = (&'a str, Rect)> + 'a {
        let mut y = 16;
        self.enabled.iter().map(move |name| {
            let height = widget_height(name);
            let p = positions.get(name).copied().unwrap_or([size.w - 296, y]);
            let r = rect(
                p[0].clamp(0, (size.w - 280).max(0)),
                p[1].clamp(0, (size.h - TASKBAR_HEIGHT - height).max(0)),
                280.min(size.w),
                height,
            );
            y += height + 12;
            (name.as_str(), r)
        })
    }
    pub fn prepare(
        &self,
        layer: &mut Layer,
        positions: &BTreeMap<String, [i32; 2]>,
        size: Size<i32, Logical>,
        accent: [f32; 4],
        theme: &str,
    ) {
        for (name, r) in self.layouts(positions, size) {
            layer.panel(r, 14.0, [0.065, 0.075, 0.10, 0.90], 1.0);
            if name == "clock" {
                layer.label(
                    &self.clock,
                    rect(r.loc.x + 20, r.loc.y + 14, 240, 56),
                    42.0,
                    1,
                    false,
                );
                layer.label(
                    &self.date,
                    rect(r.loc.x + 20, r.loc.y + 88, 240, 24),
                    12.0,
                    1,
                    false,
                );
            } else if name == "music" {
                let (title, artist) = self.music.as_ref().map_or_else(
                    || {
                        (
                            "Spotify",
                            if self.music_available {
                                "Click to browse".into()
                            } else {
                                "Music daemon not installed".into()
                            },
                        )
                    },
                    |player| (player.title(), player.artist()),
                );
                let text_x = if let Some(image) = &self.music_image {
                    layer.decoded_image(image, rect(r.loc.x + 16, r.loc.y + 12, 52, 52));
                    r.loc.x + 80
                } else {
                    r.loc.x + 16
                };
                let width = r.loc.x + r.size.w - 16 - text_x;
                layer.label(title, rect(text_x, r.loc.y + 12, width, 24), 15.0, 1, false);
                layer.label(
                    &artist,
                    rect(text_x, r.loc.y + 44, width, 20),
                    12.0,
                    1,
                    false,
                );
                for (command, b) in media_buttons(r) {
                    let enabled = self.music.as_ref().is_some_and(|p| p.allows(command));
                    layer.panel(
                        b,
                        9.0,
                        [1.0, 1.0, 1.0, if enabled { 0.16 } else { 0.05 }],
                        0.0,
                    );
                    let icon = match command {
                        crate::media::Command::Previous => "media-skip-backward-symbolic",
                        crate::media::Command::Next => "media-skip-forward-symbolic",
                        _ if self
                            .music
                            .as_ref()
                            .is_some_and(|p| p.playback == crate::media::Playback::Playing) =>
                        {
                            "media-playback-pause-symbolic"
                        }
                        _ => "media-playback-start-symbolic",
                    };
                    layer.tinted_icon(
                        icon,
                        theme,
                        rect(b.loc.x + 14, b.loc.y + 7, 16, 16),
                        crate::media::ui::ICON_COLOR,
                    );
                }
            } else if name == "media" {
                let Some(player) = self.player.as_ref() else {
                    layer.label("Nothing playing", rect(r.loc.x + 16, r.loc.y + 12, 248, 22), 14.0, 1, false);
                    continue;
                };
                layer.label(player.title(), rect(r.loc.x + 16, r.loc.y + 12, 248, 24), 15.0, 1, false);
                let artist = player.artist();
                let artist = if artist.is_empty() { player.identity.clone() } else { artist };
                layer.label(&artist, rect(r.loc.x + 16, r.loc.y + 44, 248, 20), 12.0, 1, false);
                for (command, b) in media_buttons(r) {
                    let alpha = if player.allows(command) { 0.16 } else { 0.05 };
                    layer.panel(b, 9.0, [1.0, 1.0, 1.0, alpha], 0.0);
                    layer.tinted_icon(crate::media::ui::icon(player, command), theme, rect(b.loc.x + 14, b.loc.y + 7, 16, 16), crate::media::ui::ICON_COLOR);
                }
            } else {
                layer.label(
                    "System",
                    rect(r.loc.x + 16, r.loc.y + 12, 248, 22),
                    14.0,
                    1,
                    false,
                );
                let cpu = self
                    .cpu
                    .map_or_else(|| "CPU  …".into(), |v| format!("CPU  {v}%"));
                layer.label(
                    &cpu,
                    rect(r.loc.x + 16, r.loc.y + 44, 248, 20),
                    12.0,
                    1,
                    false,
                );
                let memory = self.memory.map_or_else(
                    || "Memory  …".into(),
                    |(used, total)| {
                        format!(
                            "Memory  {:.1} / {:.1} GiB",
                            used as f64 / 1048576.0,
                            total as f64 / 1048576.0
                        )
                    },
                );
                layer.label(
                    &memory,
                    rect(r.loc.x + 16, r.loc.y + 72, 248, 20),
                    12.0,
                    1,
                    false,
                );
                let battery = self.status.battery.as_ref().map_or_else(
                    || "Battery  unavailable".into(),
                    |b| {
                        format!(
                            "Battery  {}% · {}",
                            b.percent,
                            if b.charging { "Charging" } else { "On battery" }
                        )
                    },
                );
                layer.label(
                    &battery,
                    rect(r.loc.x + 16, r.loc.y + 100, 248, 20),
                    12.0,
                    1,
                    false,
                );
                layer.label(
                    self.status.network.label(),
                    rect(r.loc.x + 16, r.loc.y + 128, 248, 20),
                    12.0,
                    1,
                    false,
                );
                let _ = accent;
            }
        }
    }
}
pub fn widget_height(name: &str) -> i32 {
    match name {
        "clock" => 132,
        "media" | "music" => 128,
        _ => 170,
    }
}
impl Gavno {
    pub(crate) fn refresh_classic_music(&mut self) {
        self.classic
            .desktop
            .widgets
            .update_music(&self.now_playing.borrow());
    }
    /// Only a visible desktop widget can pass through the open panel's input grab.
    pub(crate) fn classic_music_at(&self, point: smithay::utils::Point<f64, Logical>) -> bool {
        self.classic_desktop_available(point)
            && self.classic_desktop_local(point).is_some_and(|p| {
                self.classic
                    .desktop
                    .widgets
                    .layouts(
                        &self.classic.desktop.positions.widgets,
                        self.classic.desktop.size,
                    )
                    .any(|(name, r)| name == "music" && r.to_f64().contains(p))
            })
    }
    pub(crate) fn toggle_classic_music(&mut self) {
        let d = &self.classic.desktop;
        let Some(output) = d.output.clone() else {
            return;
        };
        let anchor = d
            .widgets
            .layouts(&d.positions.widgets, d.size)
            .find(|(name, _)| *name == "music")
            .map(|(_, r)| r);
        if d.widgets.music_available && anchor.is_some() {
            self.toggle_music_at(&output, anchor);
        }
    }
    pub fn classic_widget_gate(&self) -> Gate {
        let d = &self.classic.desktop;
        let covered = d.output.as_ref().is_some_and(|o| {
            self.space.elements().any(|w| {
                matches!(
                    crate::window_mode::mode_state(w).mode,
                    crate::window_mode::WindowMode::Maximized
                        | crate::window_mode::WindowMode::Fullscreen
                ) && self.output_for_window(w).as_ref() == Some(o)
            })
        });
        Gate {
            classic: self.features.on(Feature::Classic)
                && self.classic.mode == ShellMode::Classic
                && self.classic.settled == ShellMode::Classic
                && !self.classic.progress.is_active()
                && d.active,
            visible: !covered,
            locked: self.protocols.locks.locked || self.greeter.is_some(),
        }
    }
    pub fn reconcile_classic_widgets(&mut self) {
        let active = self.classic_widget_gate().active();
        let w = &mut self.classic.desktop.widgets;
        if !active {
            if let Some(t) = w.clock_timer.take() {
                self.loop_handle.remove(t);
            }
            if let Some(t) = w.system_timer.take() {
                self.loop_handle.remove(t);
            }
            w.refreshing = false;
            return;
        }
        w.update_status(&self.media.status);
        let player = self.now_playing.borrow().snapshot.player.clone();
        w.update_player(player.as_ref());
        if !w.refreshing {
            w.update_music(&self.now_playing.borrow());
            w.update_clock();
            w.refreshing = true;
        }
        if w.enabled.iter().any(|n| n == "clock") && w.clock_timer.is_none() {
            let delay = crate::topbar::next_minute(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default(),
            );
            if let Ok(t) =
                self.loop_handle
                    .insert_source(Timer::from_duration(delay), |_, _, gv| {
                        let _stall = crate::perf::Stall::new("timer.widget_clock");
                        gv.guard(Feature::Classic, (), |gv| {
                            gv.classic.desktop.widgets.clock_timer = None;
                            if gv.classic_widget_gate().active() {
                                gv.classic.desktop.widgets.update_clock();
                                gv.queue_redraw_all();
                                gv.reconcile_classic_widgets();
                            }
                        });
                        TimeoutAction::Drop
                    })
            {
                self.classic.desktop.widgets.clock_timer = Some(t);
            }
        }
        if self
            .classic
            .desktop
            .widgets
            .enabled
            .iter()
            .any(|n| n == "system")
            && self.classic.desktop.widgets.system_timer.is_none()
        {
            if !self.classic.desktop.widgets.sampling {
                self.classic.desktop.widgets.sampling = true;
                super::desktop_io::spawn_job(self, || super::desktop_io::Reply::System(sample()));
            }
            if let Ok(t) = self.loop_handle.insert_source(
                Timer::from_duration(Duration::from_secs(2)),
                |_, _, gv| {
                    let _stall = crate::perf::Stall::new("timer.widget_system");
                    gv.guard(Feature::Classic, (), |gv| {
                        gv.classic.desktop.widgets.system_timer = None;
                        gv.reconcile_classic_widgets();
                    });
                    TimeoutAction::Drop
                },
            ) {
                self.classic.desktop.widgets.system_timer = Some(t);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_accepts_four_unique_widgets_and_persists_music() {
        let config: super::super::ClassicConfig = toml::from_str(
            r#"widgets = ["clock", "clock", "unknown", "system", "media", "music", "music"]"#,
        )
        .unwrap();
        let mut w = Widgets::default();
        w.configure(&config.widgets);
        assert_eq!(w.enabled, ["clock", "system", "media", "music"]);
        assert_eq!(super::super::ClassicConfig::default().widgets, w.enabled);
        let mut positions = super::super::desktop::Positions::default();
        positions.widgets.insert("music".into(), [80, 90]);
        positions.validate();
        let restored: super::super::desktop::Positions =
            serde_json::from_str(&serde_json::to_string(&positions).unwrap()).unwrap();
        assert_eq!(restored.widgets["music"], [80, 90]);
        w.configure(&["music".into()]);
        assert_eq!(
            w.layouts(&restored.widgets, (1280, 800).into())
                .next()
                .unwrap()
                .1,
            rect(80, 90, 280, 128)
        );
    }
    #[test]
    fn music_content_uses_dedicated_player_and_reuses_decoded_cover() {
        let mut w = Widgets::default();
        let mut media = crate::media::Media::default();
        media.snapshot.player = Some(crate::media::Player {
            identity: "Other player".into(),
            ..Default::default()
        });
        media.snapshot.music_player = Some(crate::media::Player {
            name: crate::media::MUSIC_PLAYER.into(),
            identity: "Spotify".into(),
            metadata: crate::media::Metadata {
                title: "Song".into(),
                ..Default::default()
            },
            ..Default::default()
        });
        w.update_music(&media);
        assert_eq!(w.music.as_ref().unwrap().title(), "Song");
        assert!(
            media.music.session.is_none(),
            "the widget must not start a music worker"
        );
        let revision = w.revision;
        w.update_music(&media);
        assert_eq!(w.revision, revision);
        media.snapshot.music_image = Some(Default::default());
        w.update_music(&media);
        assert!(w.music_image.is_some());
        assert_eq!(w.revision, revision + 1);
        media.snapshot.music_available = true;
        w.update_music(&media);
        assert!(w.music_available);
        let revision = w.revision;
        media.snapshot.player = None;
        w.update_music(&media);
        assert_eq!(
            w.revision, revision,
            "other MPRIS players cannot invalidate music"
        );
    }
    #[test]
    fn media_widget_tracks_the_player_and_maps_buttons() {
        let mut w = Widgets::default();
        w.configure(&["media".into(), "clock".into(), "media".into(), "weather".into()]);
        assert_eq!(w.enabled, ["media", "clock"]);
        let before = w.revision;
        let player = crate::media::Player { name: "p".into(), can_next: true, ..Default::default() };
        w.update_player(Some(&player));
        assert!(w.revision > before);
        let after = w.revision;
        w.update_player(Some(&player));
        assert_eq!(w.revision, after, "an unchanged player must not re-prepare the layer");
        let r = rect(100, 50, 280, 128);
        assert_eq!(media_button(r, (100.0 + 16.0 + 52.0 * 2.0 + 5.0, 50.0 + 90.0).into()), Some(crate::media::Command::Next));
        assert_eq!(media_button(r, (100.0 + 16.0 + 5.0, 50.0 + 90.0).into()), Some(crate::media::Command::Previous));
        assert_eq!(media_button(r, (110.0, 60.0).into()), None);
    }
    #[test]
    fn cpu_delta_ignores_guest_and_handles_resets() {
        let a = parse_cpu("cpu 100 0 50 800 50 0 0 0 10 0\ncpu0 1 0 0 2").unwrap();
        let b = parse_cpu("cpu 120 0 60 860 60 0 0 0 90 0").unwrap();
        assert_eq!(
            a,
            Cpu {
                total: 1000,
                idle: 850
            }
        );
        assert_eq!(cpu_percent(a, b), Some(30));
        assert_eq!(cpu_percent(a, a), None);
        assert_eq!(cpu_percent(b, a), None);
        assert!(parse_cpu("cpu bad 0 0 0").is_none());
        assert_eq!(
            parse_memory("MemTotal: 1000 kB\nMemAvailable: 400 kB"),
            Some((600, 1000))
        );
    }
    #[test]
    fn refresh_sources_exist_only_on_visible_unlocked_classic_desktop() {
        for classic in [false, true] {
            for visible in [false, true] {
                for locked in [false, true] {
                    assert_eq!(
                        Gate {
                            classic,
                            visible,
                            locked
                        }
                        .active(),
                        classic && visible && !locked
                    );
                }
            }
        }
        let (mut h, root) = super::super::desktop_io::tests::setup();
        h.gv.config.classic.widgets = vec!["clock".into(), "system".into(), "media".into()];
        h.gv.classic
            .desktop
            .widgets
            .configure(&h.gv.config.classic.widgets);
        h.gv.reconcile_classic_widgets();
        assert!(h.gv.classic.desktop.widgets.clock_timer.is_some());
        assert!(h.gv.classic.desktop.widgets.system_timer.is_some());
        assert_eq!(h.gv.classic.desktop.widgets.enabled, ["clock", "system", "media"]);
        let w = h.window();
        h.gv.map_new_window(w.clone());
        h.gv.set_window_mode(&w, crate::window_mode::WindowMode::Maximized, None);
        h.gv.reconcile_classic_widgets();
        assert!(h.gv.classic.desktop.widgets.clock_timer.is_none());
        assert!(h.gv.classic.desktop.widgets.system_timer.is_none());
        h.gv.set_window_mode(&w, crate::window_mode::WindowMode::Floating, None);
        h.gv.protocols.locks.locked = true;
        h.gv.reconcile_classic_widgets();
        assert!(h.gv.classic.desktop.widgets.clock_timer.is_none());
        h.gv.protocols.locks.locked = false;
        h.gv.classic.mode = ShellMode::Gavno;
        h.gv.reconcile_classic_widgets();
        assert!(h.gv.classic.desktop.widgets.system_timer.is_none());
        h.gv.stop_classic_desktop();
        std::thread::sleep(Duration::from_millis(30));
        let _ = std::fs::remove_dir_all(root);
    }
}
