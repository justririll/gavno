//! Shell input, worker messages, media repeat and one-shot OSD integration.
use super::{
    osd,
    status::{self, Command, Event, Media},
    Gavno,
};
use smithay::{
    input::keyboard::keysyms,
    output::Output,
    reexports::calloop::timer::{TimeoutAction, Timer},
    utils::{Logical, Point},
};
use std::time::Duration;
#[derive(Default)]
pub struct MediaState {
    pub status: status::Snapshot,
    /// The panel owns master volume after its first successful audio snapshot.
    /// Inner None means the audio server currently has no default output.
    pub panel_volume: Option<Option<status::Volume>>,
    pub brightness: crate::brightness::State,
    pub worker: Option<std::sync::mpsc::Sender<Command>>,
    pub worker_started: bool,
    pub status_visible: bool,
    pub osd: osd::Osd,
    pub osd_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    pub repeat: Option<(u32, Media, smithay::reexports::calloop::RegistrationToken)>,
    pub core_keys: Vec<u32>,
}
impl Gavno {
    pub(crate) fn sync_brightness(&mut self) {
        self.media.brightness.configure(self.config.display.brightness_control, self.start_time.elapsed());
        self.media.status.brightness = self.media.brightness.snapshot();
        self.topbar.status.brightness = self.media.status.brightness.clone();
    }
    pub(crate) fn refresh_bar_status(&mut self, visible: bool) {
        self.sync_brightness();
        self.start_panel_controls();
        if !crate::panel_controls::fake_enabled() && !self.media.worker_started && !matches!(self.backend, crate::backend::Backend::None) {
            self.media.worker_started = true;
            self.media.status_visible = !visible;
            self.media.worker = status::spawn(&self.loop_handle);
        }
        if self.media.status_visible != visible {
            self.media.status_visible = visible;
            if let Some(worker) = &self.media.worker {
                let _ = worker.send(Command::Visible(visible));
            }
        }
    }
    pub(crate) fn bar_status(&mut self, event: Event) {
        match event {
            Event::Mic(v) => self.show_bar_osd(osd::Kind::Microphone, v.level, v.muted),
            Event::Status(mut snapshot) => {
                if let Some(volume) = self.media.panel_volume {
                    snapshot.volume = volume;
                }
                self.media.brightness.backlight = snapshot.brightness.take();
                self.media.brightness.configure(self.config.display.brightness_control, self.start_time.elapsed());
                snapshot.brightness = self.media.brightness.snapshot();
                let volume_changed = self.media.status.volume != snapshot.volume;
                let brightness_changed = self.media.status.brightness != snapshot.brightness;
                self.guard(crate::module::Feature::Bar, (), |gv| gv.topbar.status = snapshot.clone());
                self.media.status = snapshot;
                let battery = self.media.status.battery.clone();
                self.battery_notification(battery.as_ref());
                // A status refresh can correct visible feedback, but never opens
                // an OSD on its own.
                if self.media.osd.deadline.is_some() {
                    if volume_changed {
                        let v = self.media.status.volume.unwrap_or(status::Volume {
                            level: 0.0,
                            muted: true,
                        });
                        self.show_bar_osd(osd::Kind::Volume, v.level, v.muted);
                    }
                    if brightness_changed {
                        if let Some(b) = &self.media.status.brightness {
                            self.show_bar_osd(osd::Kind::Brightness, b.level(), false);
                        }
                    }
                }
                if self.media.status_visible || brightness_changed {
                    self.queue_redraw_all();
                }
            }
        }
    }
    pub(crate) fn update_panel_volume(&mut self, volume: Option<status::Volume>) {
        let changed = self.media.status.volume != volume;
        self.media.panel_volume = Some(volume);
        self.topbar.set_panel_volume(volume);
        self.media.status.volume = volume;
        if changed {
            // Hotkeys still use the legacy command worker. Its cached status
            // cannot correct their OSD now; the panel's audio event does that.
            if self.media.osd.deadline.is_some() {
                let v = volume.unwrap_or(status::Volume { level: 0.0, muted: true });
                self.show_bar_osd(osd::Kind::Volume, v.level, v.muted);
            }
            self.queue_redraw_all();
        }
    }
    fn show_bar_osd(&mut self, kind: osd::Kind, value: f64, muted: bool) {
        if let Some(t) = self.media.osd_timer.take() {
            self.loop_handle.remove(t);
        }
        let now = self.start_time.elapsed();
        self.media.osd.show(kind, value, muted, now);
        match self.loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(1500)),
            |_, _, gv| {
                let _stall = crate::perf::Stall::new("timer.bar_osd");
                gv.media.osd_timer = None;
                gv.media.osd.expire(gv.start_time.elapsed());
                gv.queue_redraw_all();
                TimeoutAction::Drop
            },
        ) {
            Ok(t) => self.media.osd_timer = Some(t),
            Err(error) => tracing::warn!(%error,"cannot schedule OSD dismissal"),
        }
        self.queue_redraw_all();
    }
    pub fn bar_media(&mut self, action: Media) {
        self.refresh_bar_status(self.media.status_visible);
        if self.media.brightness.software && matches!(action, Media::BrightnessUp | Media::BrightnessDown) {
            let step = if action == Media::BrightnessUp { 0.05 } else { -0.05 };
            self.set_software_brightness(self.media.brightness.level() + step);
            return;
        }
        if let Some(worker) = &self.media.worker {
            let _ = worker.send(Command::Media(action));
        }
        match action {
            Media::BrightnessUp | Media::BrightnessDown => {
                if let Some(b) = &self.media.status.brightness {
                    let level = b.level();
                    self.show_bar_osd(osd::Kind::Brightness, level, false);
                }
            }
            Media::MicMute => {}
            _ => {
                let v = self.media.status.volume.unwrap_or(status::Volume {
                    level: 0.0,
                    muted: false,
                });
                let level = v.level;
                self.show_bar_osd(osd::Kind::Volume, level, v.muted);
            }
        }
    }
    /// Core keys are filtered before the lock, shortcut inhibitors and clients.
    pub fn bar_key(&mut self, sym: u32, code: u32, pressed: bool) -> bool {
        if !pressed {
            let consumed = self.media.core_keys.contains(&code);
            self.media.core_keys.retain(|key| *key != code);
            if self
                .media
                .repeat
                .as_ref()
                .is_some_and(|(key, _, _)| *key == code)
            {
                let (_, _, token) = self.media.repeat.take().unwrap();
                self.loop_handle.remove(token);
            }
            return consumed;
        }
        if self.media.core_keys.contains(&code) {
            return true;
        }
        let playback = match sym {
            keysyms::KEY_XF86AudioPlay | keysyms::KEY_XF86AudioPause => Some(crate::media::Command::PlayPause),
            keysyms::KEY_XF86AudioNext => Some(crate::media::Command::Next),
            keysyms::KEY_XF86AudioPrev => Some(crate::media::Command::Previous),
            _ => None,
        };
        if let Some(command) = playback {
            if !self.features.on(crate::module::Feature::Media) { return false; }
            self.media.core_keys.push(code);
            self.media_command(command);
            return true;
        }
        let media = match sym {
            keysyms::KEY_XF86AudioRaiseVolume => Media::VolumeUp,
            keysyms::KEY_XF86AudioLowerVolume => Media::VolumeDown,
            keysyms::KEY_XF86AudioMute => Media::Mute,
            keysyms::KEY_XF86AudioMicMute => Media::MicMute,
            keysyms::KEY_XF86MonBrightnessUp => Media::BrightnessUp,
            keysyms::KEY_XF86MonBrightnessDown => Media::BrightnessDown,
            _ => {
                if self.features.on(crate::module::Feature::Bar) && self.topbar.menu.is_some()
                    && matches!(
                        sym,
                        keysyms::KEY_Escape | keysyms::KEY_Super_L | keysyms::KEY_Super_R
                    )
                {
                    self.guard(crate::module::Feature::Bar, (), |gv| gv.topbar.close());
                    self.media.core_keys.push(code);
                    self.guard(crate::module::Feature::Gestures, (), |gv| gv.gestures.super_tap.cancel());
                    self.queue_redraw_all();
                    return true;
                }
                return false;
            }
        };
        self.media.core_keys.push(code);
        if let Some((_, _, token)) = self.media.repeat.take() {
            self.loop_handle.remove(token);
        }
        self.bar_media(media);
        if self.config.repeat_rate > 0 {
            let delay = Duration::from_millis(self.config.repeat_delay as u64);
            match self
                .loop_handle
                .insert_source(Timer::from_duration(delay), move |_, _, gv| {
                    let _stall = crate::perf::Stall::new("timer.media_repeat");
                    if gv
                        .media
                        .repeat
                        .as_ref()
                        .is_some_and(|(key, _, _)| *key == code)
                        && gv.config.repeat_rate > 0
                    {
                        gv.bar_media(media);
                        TimeoutAction::ToDuration(Duration::from_secs_f64(
                            1.0 / gv.config.repeat_rate as f64,
                        ))
                    } else {
                        gv.media.repeat = None;
                        TimeoutAction::Drop
                    }
                }) {
                Ok(t) => self.media.repeat = Some((code, media, t)),
                Err(error) => tracing::warn!(%error,"cannot schedule media key repeat"),
            }
        }
        true
    }
    pub fn bar_motion(&mut self, pos: Point<f64, Logical>) -> () {
        self.guard(crate::module::Feature::Bar, (), |gv| gv.bar_motion_feature(pos))
    }
    fn bar_motion_feature(&mut self, pos: Point<f64, Logical>) -> () {
        if self.protocols.locks.locked {
            return;
        }
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        let mut changed = false;
        for o in outputs {
            let Some(geo) = self.space.output_geometry(&o) else {
                continue;
            };
            let local = pos - geo.loc.to_f64();
            let hit = if geo.to_f64().contains(pos) {
                self.topbar.action(&o, local)
            } else {
                None
            };
            if let Some(v) = self.topbar.views.iter_mut().find(|v| v.output == o) {
                if v.hover != hit {
                    v.hover = hit;
                    changed = true;
                }
            }
        }
        if changed {
            self.queue_redraw_all();
        }
    }
    fn set_software_brightness(&mut self, level: f64) {
        self.media.brightness.set(level, self.start_time.elapsed());
        self.sync_brightness();
        self.show_bar_osd(osd::Kind::Brightness, self.media.brightness.level(), false);
    }
    /// Brightness from the quick settings slider. Backlight or software
    /// dimming, without an OSD: the slider is its own feedback.
    pub(crate) fn panel_set_brightness(&mut self, level: f64) {
        self.set_brightness(level, false);
    }
    fn set_brightness(&mut self, level: f64, osd: bool) {
        self.sync_brightness();
        if self.media.brightness.software {
            self.media.brightness.set(level, self.start_time.elapsed());
            self.sync_brightness();
            if osd {
                self.show_bar_osd(osd::Kind::Brightness, self.media.brightness.level(), false);
            }
            self.queue_redraw_all();
            return;
        }
        let actual = if let Some(b) = self.media.status.brightness.as_mut() {
            b.value = ((b.max as f64 * level).round() as u32).clamp(b.min_value(), b.max);
            b.level()
        } else {
            level
        };
        self.media.brightness.backlight = self.media.status.brightness.clone();
        if osd {
            self.show_bar_osd(osd::Kind::Brightness, actual, false);
        }
        if let Some(worker) = &self.media.worker {
            let _ = worker.send(Command::Brightness(level));
        }
        self.guard(crate::module::Feature::Bar, (), |gv| gv.topbar.status = gv.media.status.clone());
        self.queue_redraw_all();
    }
    pub fn bar_axis(&mut self, output: &Output, p: Point<f64, Logical>, delta: f64) -> bool {
        self.guard(crate::module::Feature::Bar, false, |gv| gv.bar_axis_feature(output, p, delta))
    }
    fn bar_axis_feature(&mut self, output: &Output, p: Point<f64, Logical>, delta: f64) -> bool {
        let Some(v) = self.topbar.views.iter().find(|v| &v.output == output) else {
            return false;
        };
        let Some(l) = &v.layout else {
            return false;
        };
        if l.status.to_f64().contains(p) {
            if self.topbar.controls.master_step(delta) { return true; }
            if delta != 0.0 {
                self.bar_media(if delta < 0.0 {
                    Media::VolumeUp
                } else {
                    Media::VolumeDown
                });
            }
            return true;
        }
        if l.workspaces.to_f64().contains(p) {
            if delta == 0.0 || delta.signum() != self.topbar.scroll.signum() {
                self.topbar.scroll = 0.0;
            }
            self.topbar.scroll += delta;
            if self.topbar.scroll.abs() >= 30.0 {
                let o = self.workspaces.ensure(output);
                let next = if self.topbar.scroll < 0.0 {
                    o.list.active.saturating_sub(1)
                } else {
                    (o.list.active + 1).min(o.list.items.len() - 1)
                };
                self.topbar.scroll = 0.0;
                self.switch_workspace(output, next);
            }
            return true;
        }
        p.y < super::PANEL as f64 || self.topbar.menu.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        state::tests::Harness,
        topbar::{BarAction, BarView, Canvas},
    };
    use smithay::output::{Mode, PhysicalProperties, Subpixel};
    use std::sync::mpsc;
    fn setup() -> (Harness, Output) {
        let mut h = Harness::new();
        let output = Output::new(
            "bar-test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: (1646, 926).into(),
                refresh: 120000,
            }),
            None,
            None,
            None,
        );
        h.gv.space.map_output(&output, (0, 0));
        h.gv.workspaces.ensure(&output);
        (h, output)
    }
    #[test]
    fn player_channel_exposes_capsule_and_click_sends_play_pause() {
        for width in [1280, 1646] {
            let (mut h, o) = setup();
            o.change_current_state(Some(Mode { size: (width, 926).into(), refresh: 120000 }), None, None, None);
            let (tx, rx) = mpsc::channel();
            h.gv.now_playing.borrow_mut().worker = Some(tx);
            let events = h.gv.media_events().unwrap();
            events.send(crate::media::Snapshot {
                player: Some(crate::media::Player {
                    name: "org.mpris.MediaPlayer2.test".into(),
                    metadata: crate::media::Metadata { title: "Track".into(), artists: vec!["Artist".into()], ..Default::default() },
                    playback: crate::media::Playback::Playing, can_play: true, can_pause: true,
                    can_next: true, can_previous: true, ..Default::default()
                }), ..Default::default()
            }).unwrap();
            h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
            let (layout, snapshot) = super::super::media_layout(&h.gv.topbar.now_playing, width, 55, true, true, &h.gv.features, true);
            assert_eq!(snapshot.player.as_ref().unwrap().label(), "Track — Artist");
            let area = layout.media.expect("player update exposes capsule");
            let mut canvas = Canvas::default();
            canvas.scale = 1.75;
            let mut hits = Vec::new();
            super::super::push_hit(&canvas, &mut hits, area, BarAction::Media(crate::media::Command::PlayPause));
            h.gv.topbar.views.push(BarView {
                output: o.clone(), canvas, dots: vec![], hits, visible: true, hover: None,
                layout: Some(layout), popover: None,
            });
            // Exercise the shell's real pointer dispatch with a point inside the capsule.
            let pointer = h.gv.seat.get_pointer().unwrap();
            pointer.motion(&mut h.gv, None, &smithay::input::pointer::MotionEvent {
                location: (area.loc.x as f64 + 10.0, 15.0).into(),
                serial: smithay::utils::SERIAL_COUNTER.next_serial(), time: 1,
            });
            assert!(h.gv.shell_contains(h.gv.pointer_location()));
            assert!(h.gv.shell_button(0x110, true));
            assert!(matches!(rx.try_recv(), Ok(crate::media::mpris::Request::Command(crate::media::Command::PlayPause))));
            h.gv.protocols.locks.locked = true;
            for (i, (sym, expected)) in [
                (keysyms::KEY_XF86AudioPlay, crate::media::Command::PlayPause),
                (keysyms::KEY_XF86AudioPause, crate::media::Command::PlayPause),
                (keysyms::KEY_XF86AudioNext, crate::media::Command::Next),
                (keysyms::KEY_XF86AudioPrev, crate::media::Command::Previous),
            ].into_iter().enumerate() {
                let code = 200 + i as u32;
                assert!(h.gv.bar_key(sym, code, true));
                assert!(h.gv.bar_key(sym, code, true));
                assert!(h.gv.bar_key(0, code, false));
                assert!(matches!(rx.try_recv(), Ok(crate::media::mpris::Request::Command(c)) if c == expected));
                assert!(rx.try_recv().is_err());
                assert!(h.gv.media.repeat.is_none());
            }
            h.gv.features.inject_panic(crate::module::Feature::Media);
            h.gv.media_command(crate::media::Command::PlayPause);
            assert!(h.gv.now_playing.borrow().snapshot.player.is_none());
            assert!(h.gv.topbar.views[0].hits.iter().all(|(_, action)| !matches!(action, BarAction::Media(_))));
            assert!(!h.gv.bar_key(keysyms::KEY_XF86AudioNext, 250, true));
            events.send(snapshot).unwrap();
            h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
            assert!(h.gv.now_playing.borrow().snapshot.player.is_none());
        }
    }
    #[test]
    fn core_keys_work_locked_and_repeat_is_cancelled_on_release() {
        let (mut h, _) = setup();
        let (tx, rx) = mpsc::channel();
        h.gv.media.worker = Some(tx);
        h.gv.protocols.locks.locked = true;
        assert!(h.gv.bar_key(keysyms::KEY_XF86AudioRaiseVolume, 123, true));
        assert!(matches!(rx.try_recv(), Ok(Command::Media(Media::VolumeUp))));
        assert!(h.gv.media.repeat.is_some());
        assert!(h.gv.media.osd_timer.is_some());
        assert!(h.gv.is_animating());
        // XKB may report a different keysym on release after a layout change.
        assert!(h.gv.bar_key(0, 123, false));
        assert!(h.gv.media.repeat.is_none());
        // Advance in frame steps to simulate the finite fade, rather than
        // asking a spring to integrate a capped two-second catch-up step.
        let base = h.gv.start_time.elapsed();
        for frame in 0..=250 {
            h.gv.media
                .osd
                .advance(base + Duration::from_millis(frame * 8));
        }
        assert!(!h.gv.is_animating());
    }
    #[test]
    fn software_keys_and_slider_never_send_backlight_commands() {
        let (mut h, _) = setup();
        let (tx, rx) = mpsc::channel();
        h.gv.media.worker = Some(tx);
        h.gv.media.worker_started = true;
        h.gv.config.display.brightness_control = crate::brightness::Control::Software;
        let hardware = status::Brightness { name: "test-panel".into(), value: 80, max: 100 };
        h.gv.bar_status(Event::Status(status::Snapshot { brightness: Some(hardware.clone()), ..Default::default() }));
        h.gv.set_brightness(0.5, true);
        assert_eq!(h.gv.media.brightness.level(), 0.5);
        h.gv.protocols.locks.locked = true;
        assert!(h.gv.bar_key(keysyms::KEY_XF86MonBrightnessDown, 240, true));
        assert!(h.gv.bar_key(0, 240, false));
        assert!((h.gv.media.brightness.level() - 0.45).abs() < 1e-10);
        h.gv.bar_media(Media::BrightnessUp);
        assert_eq!(h.gv.topbar.status.brightness.as_ref().unwrap().level(), 0.5);
        h.gv.set_brightness(0.0, true);
        h.gv.bar_media(Media::BrightnessDown);
        assert_eq!(h.gv.media.brightness.level(), 0.05);
        h.gv.set_brightness(2.0, true);
        assert_eq!(h.gv.media.brightness.level(), 1.0);
        // An asynchronous hardware refresh must not replace software feedback.
        h.gv.bar_status(Event::Status(status::Snapshot { brightness: Some(hardware.clone()), ..Default::default() }));
        assert_eq!(h.gv.topbar.status.brightness.as_ref().unwrap().level(), 1.0);
        assert!(rx.try_recv().is_err());
        let mut next = h.gv.config.clone();
        next.display.brightness_control = crate::brightness::Control::Backlight;
        let prepared = crate::config_reload::Prepared::new(next, &h.gv.config).unwrap();
        let report = crate::config_reload::apply(&mut h.gv, prepared).unwrap();
        assert!(report["applied"].as_array().unwrap().contains(&serde_json::json!("display.brightness_control")));
        assert_eq!(h.gv.topbar.status.brightness, Some(hardware));
        assert!(!h.gv.media.brightness.software);
        assert!(!h.gv.media.brightness.active());
        h.gv.set_brightness(0.3, true);
        assert!(matches!(rx.try_recv(), Ok(Command::Brightness(level)) if level == 0.3));
    }
    #[test]
    fn visibility_commands_only_follow_transitions() {
        let (mut h, _) = setup();
        let (tx, rx) = mpsc::channel();
        h.gv.media.worker = Some(tx);
        h.gv.media.worker_started = true;
        h.gv.refresh_bar_status(true);
        assert!(matches!(rx.try_recv(), Ok(Command::Visible(true))));
        h.gv.refresh_bar_status(true);
        assert!(rx.try_recv().is_err());
        h.gv.refresh_bar_status(false);
        assert!(matches!(rx.try_recv(), Ok(Command::Visible(false))));
        h.gv.refresh_bar_status(false);
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn popovers_are_exclusive_and_escape_and_super_close_them() {
        let (mut h, o) = setup();
        h.gv.bar_action(&o, BarAction::PowerMenu);
        assert!(h.gv.panel_controls_open());
        h.gv.bar_action(&o, BarAction::Calendar);
        assert!(!h.gv.panel_controls_open());
        assert_eq!(h.gv.topbar.menu, Some(o.clone()));
        assert!(h.gv.bar_key(keysyms::KEY_Escape, 9, true));
        assert!(h.gv.topbar.menu.is_none());
        // Quick settings take the keyboard while open, so Escape reaches the
        // panel; Super is the one the core filter still closes.
        h.gv.bar_action(&o, BarAction::PowerMenu);
        assert!(h.gv.panel_controls_open());
        h.gv.bar_action(&o, BarAction::Calendar);
        assert!(h.gv.bar_key(keysyms::KEY_Super_L, 133, true));
        assert!(h.gv.topbar.menu.is_none());
        assert!(h.gv.bar_key(keysyms::KEY_Super_L, 133, false));
        assert!(!h.gv.media.core_keys.contains(&133));
    }
}
