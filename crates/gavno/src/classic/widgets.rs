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
    music_image: Option<std::sync::Arc<crate::media::ui::Cover>>,
    music_available: bool,
}
const MEDIA_COMMANDS: [crate::media::Command; 3] = [
    crate::media::Command::Previous,
    crate::media::Command::PlayPause,
    crate::media::Command::Next,
];
/// Previous / play-pause / next buttons inside a media widget panel.
pub(crate) fn media_buttons(
    r: Rect,
    music: bool,
) -> impl Iterator<Item = (crate::media::Command, Rect)> {
    let center = if music {
        r.loc.x + 128 + (r.size.w - 142) / 2
    } else {
        r.loc.x + r.size.w / 2
    };
    MEDIA_COMMANDS
        .into_iter()
        .enumerate()
        .map(move |(index, command)| {
            let diameter = if command == crate::media::Command::PlayPause {
                36
            } else {
                30
            };
            (
                command,
                rect(
                    center + (index as i32 - 1) * 42 - diameter / 2,
                    r.loc.y + 86 - diameter / 2,
                    diameter,
                    diameter,
                ),
            )
        })
}
/// Drawing and input share the same transport geometry for each widget kind.
pub fn media_button(
    r: Rect,
    p: smithay::utils::Point<f64, Logical>,
    music: bool,
) -> Option<crate::media::Command> {
    media_buttons(r, music)
        .find(|(_, r)| r.to_f64().contains(p))
        .map(|(command, _)| command)
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
        let (player, image) = media.music_now();
        let available = media.snapshot.music_available || media.music.session.is_some();
        if self.music != player
            || !crate::media::ui::same_cover(&self.music_image, &image)
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
        // "media" (any player) and "music" (the music daemon) would show the
        // same thing twice: keep the one that can play something here.
        let both = self.enabled.iter().any(|n| n == "media") && self.enabled.iter().any(|n| n == "music");
        let hidden = if !both { "" } else if self.music_available { "media" } else { "music" };
        self.enabled.iter().filter(move |name| name.as_str() != hidden).map(move |name| {
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
            layer.glass(
                r,
                if name == "music" || name == "media" {
                    16.0
                } else {
                    14.0
                },
                [0.065, 0.075, 0.10, 0.90],
            );
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
            } else if name == "music" || name == "media" {
                use crate::ui::text::{TextStyleSpec, Weight};
                let music = name == "music";
                let player = if music {
                    self.music.as_ref()
                } else {
                    self.player.as_ref()
                };
                if music {
                    if let Some(image) = &self.music_image {
                        layer.panel(
                            r,
                            16.0,
                            [image.tint[0], image.tint[1], image.tint[2], 0.30],
                            0.0,
                        );
                        layer.decoded_image(
                            &image.buffer,
                            rect(r.loc.x + 14, r.loc.y + 14, 100, 100),
                        );
                    } else {
                        layer.panel(
                            rect(r.loc.x + 14, r.loc.y + 14, 100, 100),
                            10.0,
                            [1.0, 1.0, 1.0, 0.07],
                            0.0,
                        );
                        layer.tinted_icon_alpha(
                            "audio-x-generic-symbolic",
                            theme,
                            rect(r.loc.x + 46, r.loc.y + 46, 36, 36),
                            [255; 3],
                            0.35,
                        );
                    }
                }
                let x = r.loc.x + if music { 128 } else { 16 };
                let width = r.size.w - if music { 142 } else { 32 };
                let title = player.map_or(if music { "Spotify" } else { "Nothing playing" }, |p| {
                    p.title()
                });
                let artist = player.map_or_else(
                    || {
                        if music {
                            if self.music_available {
                                "Click to browse".into()
                            } else {
                                "Music daemon not installed".into()
                            }
                        } else {
                            String::new()
                        }
                    },
                    |p| {
                        let artist = p.artist();
                        if !music && artist.is_empty() {
                            p.identity.clone()
                        } else {
                            artist
                        }
                    },
                );
                layer.styled_label(
                    title,
                    rect(x, r.loc.y + 16, width, 24),
                    TextStyleSpec {
                        size: 14.0,
                        weight: Weight::SemiBold,
                        color: [
                            1.0,
                            1.0,
                            1.0,
                            if !music && player.is_none() {
                                0.55
                            } else {
                                0.97
                            },
                        ],
                        tracking: 0.0,
                    },
                    1,
                    false,
                );
                layer.styled_label(
                    &artist,
                    rect(x, r.loc.y + 38, width, 20),
                    TextStyleSpec {
                        size: 11.5,
                        weight: Weight::Regular,
                        color: [1.0, 1.0, 1.0, if player.is_some() { 0.62 } else { 0.55 }],
                        tracking: 0.0,
                    },
                    1,
                    false,
                );
                for (command, area) in media_buttons(r, music) {
                    crate::media::ui::layer_transport(layer, theme, player, command, area);
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
    /// Overlay the daemon session on the MPRIS snapshot and show the result in
    /// both media widgets, so they always match the bar.
    pub(crate) fn refresh_classic_music(&mut self) {
        let mut media = self.now_playing.borrow_mut();
        media.sync_music_session();
        let widgets = &mut self.classic.desktop.widgets;
        widgets.update_player(media.snapshot.player.as_ref());
        widgets.update_music(&media);
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
        let active = !self.power.is_off() && self.classic_widget_gate().active();
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
        w.configure(&[
            "media".into(),
            "clock".into(),
            "media".into(),
            "weather".into(),
        ]);
        assert_eq!(w.enabled, ["media", "clock"]);
        let before = w.revision;
        let player = crate::media::Player {
            name: "p".into(),
            can_next: true,
            ..Default::default()
        };
        w.update_player(Some(&player));
        assert!(w.revision > before);
        let after = w.revision;
        w.update_player(Some(&player));
        assert_eq!(
            w.revision, after,
            "an unchanged player must not re-prepare the layer"
        );
        let r = rect(100, 50, 280, 128);
        for music in [false, true] {
            for (command, area) in media_buttons(r, music) {
                let point = (
                    area.loc.x as f64 + area.size.w as f64 / 2.0,
                    area.loc.y as f64 + area.size.h as f64 / 2.0,
                )
                    .into();
                assert_eq!(media_button(r, point, music), Some(command));
            }
            assert_eq!(media_button(r, (110.0, 60.0).into(), music), None);
        }
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
        assert_eq!(
            h.gv.classic.desktop.widgets.enabled,
            ["clock", "system", "media"]
        );
        h.gv.power_off_monitors("test");
        assert!(h.gv.classic.desktop.widgets.clock_timer.is_none());
        assert!(h.gv.classic.desktop.widgets.system_timer.is_none());
        h.gv.power_on_monitors("test");
        h.gv.reconcile_classic_widgets();
        assert!(h.gv.classic.desktop.widgets.clock_timer.is_some());
        assert!(h.gv.classic.desktop.widgets.system_timer.is_some());
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

#[cfg(test)]
mod music_design_artifacts {
    use super::*;
    use crate::media::ui::artifacts;
    #[test]
    fn classic_music_and_media_artifacts() {
        let mut renderer = artifacts::renderer();
        let path = artifacts::cover_path("widget-art");
        for name in ["music", "media"] {
            for empty in [false, true] {
                let mut widgets = Widgets {
                    enabled: vec![name.into()],
                    music_available: true,
                    ..Default::default()
                };
                if !empty {
                    widgets.player = Some(artifacts::player());
                    widgets.music = Some(artifacts::player());
                    widgets.music_image = Some(artifacts::decoded_cover(&path));
                }
                for scale in [1.0, 1.35] {
                    let mut layer = Layer::default();
                    layer.begin(scale);
                    widgets.prepare(
                        &mut layer,
                        &BTreeMap::from([(name.into(), [20, 20])]),
                        (320, 220).into(),
                        [0.22, 0.58, 0.42, 1.0],
                        "Adwaita",
                    );
                    let mut elements = Vec::new();
                    layer.append(&mut renderer, &mut elements, 1.0, (0.0, 0.0).into());
                    artifacts::capture(
                        &mut renderer,
                        &elements,
                        (320, 168),
                        scale,
                        &format!("classic-{name}{}", if empty { "-empty" } else { "" }),
                    );
                }
            }
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod media_design_tests {
    use super::*;
    use crate::media::{
        ui::{artifacts, Cover},
        Command,
    };
    #[test]
    fn transport_drawing_and_clicks_share_rectangles_and_cover_controls_tint() {
        let r = rect(20, 20, 280, 128);
        for music in [true, false] {
            let name = if music { "music" } else { "media" };
            let widgets = Widgets {
                enabled: vec![name.into()],
                music: Some(artifacts::player()),
                player: Some(artifacts::player()),
                ..Default::default()
            };
            let mut layer = Layer::default();
            layer.begin(1.0);
            widgets.prepare(
                &mut layer,
                &BTreeMap::from([(name.into(), [20, 20])]),
                (640, 480).into(),
                [0.3; 4],
                "Adwaita",
            );
            let centers: Vec<_> = media_buttons(r, music)
                .map(|(_, a)| a.loc.x + a.size.w / 2)
                .collect();
            assert_eq!(centers[1] - centers[0], 42);
            assert_eq!(centers[2] - centers[1], 42);
            assert_eq!(centers[1], r.loc.x + if music { 197 } else { 140 });
            for (command, area) in media_buttons(r, music) {
                assert_eq!(area.loc.y + area.size.h / 2, r.loc.y + 86);
                let side = if command == Command::PlayPause {
                    18
                } else {
                    16
                };
                let icon = rect(
                    area.loc.x + (area.size.w - side) / 2,
                    area.loc.y + (area.size.h - side) / 2,
                    side,
                    side,
                );
                assert!(
                    layer.image_rects().contains(&icon),
                    "missing {command:?} at {icon:?}"
                );
                let point = (
                    area.loc.x as f64 + area.size.w as f64 / 2.0,
                    area.loc.y as f64 + area.size.h as f64 / 2.0,
                )
                    .into();
                assert_eq!(media_button(r, point, music), Some(command));
                if command == Command::PlayPause {
                    assert!(layer
                        .panels()
                        .iter()
                        .any(|(r, c)| *r == area && c[3] == 0.96));
                }
            }
            assert_eq!(
                layer.panels().iter().filter(|(area, _)| *area == r).count(),
                1
            );
        }
        let art = std::sync::Arc::new(Cover {
            tint: [0.2, 0.3, 0.4],
            ..Default::default()
        });
        let mut widgets = Widgets {
            enabled: vec!["music".into()],
            music_image: Some(art.clone()),
            ..Default::default()
        };
        let positions = BTreeMap::from([("music".into(), [20, 20])]);
        let mut layer = Layer::default();
        layer.begin(1.0);
        widgets.prepare(
            &mut layer,
            &positions,
            (640, 480).into(),
            [0.3; 4],
            "Adwaita",
        );
        assert!(layer.panels().contains(&(r, [0.2, 0.3, 0.4, 0.30])));
        assert_eq!(
            layer.panels().iter().filter(|(area, _)| *area == r).count(),
            2
        );
        widgets.music_image = None;
        layer.begin(1.0);
        widgets.prepare(
            &mut layer,
            &positions,
            (640, 480).into(),
            [0.3; 4],
            "Adwaita",
        );
        assert_eq!(
            layer.panels().iter().filter(|(area, _)| *area == r).count(),
            1
        );
    }
    #[test]
    fn unchanged_art_does_not_invalidate_but_a_new_cover_does() {
        let mut widgets = Widgets::default();
        let mut media = crate::media::Media::default();
        media.snapshot.music_player = Some(artifacts::player());
        media.snapshot.music_image = Some(std::sync::Arc::new(Cover::default()));
        widgets.update_music(&media);
        let revision = widgets.revision;
        for _ in 0..10 {
            widgets.update_music(&media);
        }
        assert_eq!(widgets.revision, revision);
        media.snapshot.music_image = Some(std::sync::Arc::new(Cover {
            tint: [0.2, 0.3, 0.4],
            ..Default::default()
        }));
        widgets.update_music(&media);
        assert_eq!(widgets.revision, revision + 1);
    }
}
