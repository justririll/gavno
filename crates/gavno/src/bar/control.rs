//! Shell input, worker messages, media repeat and one-shot OSD integration.
use super::{
    osd,
    status::{self, Command, Event, Media},
    BarAction, Gavno,
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
    pub worker: Option<std::sync::mpsc::Sender<Command>>,
    pub worker_started: bool,
    pub status_visible: bool,
    pub osd: osd::Osd,
    pub osd_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    pub repeat: Option<(u32, Media, smithay::reexports::calloop::RegistrationToken)>,
    pub core_keys: Vec<u32>,
}
impl Gavno {
    pub(crate) fn refresh_bar_status(&mut self, visible: bool) {
        if !self.media.worker_started && !matches!(self.backend, crate::backend::Backend::None) {
            self.media.worker_started = true;
            self.media.status_visible = !visible;
            self.media.worker = status::spawn(
                &self.loop_handle,
                matches!(self.backend, crate::backend::Backend::Udev(_)),
            );
        }
        if self.media.status_visible != visible {
            self.media.status_visible = visible;
            if let Some(worker) = &self.media.worker {
                let _ = worker.send(Command::Visible(visible));
            }
        }
    }
    pub(super) fn bar_status(&mut self, event: Event) {
        match event {
            Event::Mic(v) => self.show_bar_osd(osd::Kind::Microphone, v.level, v.muted),
            Event::Status(snapshot) => {
                let volume_changed = self.media.status.volume != snapshot.volume;
                let brightness_changed = self.media.status.brightness != snapshot.brightness;
                self.guard(crate::module::Feature::Bar, (), |gv| gv.topbar.status = snapshot.clone());
                self.media.status = snapshot;
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
                if self.media.status_visible {
                    self.queue_redraw_all();
                }
            }
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
        self.bar_drag(pos);
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
    pub fn bar_drag(&mut self, pos: Point<f64, Logical>) -> () {
        self.guard(crate::module::Feature::Bar, (), |gv| gv.bar_drag_feature(pos))
    }
    fn bar_drag_feature(&mut self, pos: Point<f64, Logical>) -> () {
        let Some((output, action)) = self.topbar.drag.clone() else {
            return;
        };
        let Some(geo) = self.space.output_geometry(&output) else {
            return;
        };
        let local = pos - geo.loc.to_f64();
        let Some(v) = self.topbar.views.iter().find(|v| v.output == output) else {
            return;
        };
        let Some((r, _)) = v.hits.iter().find(|(_, a)| a == &action) else {
            return;
        };
        let level = ((local.x - r.loc.x - 7.0) / (r.size.w - 14.0)).clamp(0.0, 1.0);
        self.bar_set_slider(action, level);
    }
    fn bar_set_slider(&mut self, action: BarAction, level: f64) {
        let command = if action == BarAction::Volume {
            let muted = self.media.status.volume.is_some_and(|v| v.muted);
            self.media.status.volume = Some(status::Volume { level, muted });
            self.show_bar_osd(osd::Kind::Volume, level, muted);
            Command::Volume(level)
        } else {
            let actual = if let Some(b) = self.media.status.brightness.as_mut() {
                b.value = ((b.max as f64 * level).round() as u32).clamp(b.min_value(), b.max);
                b.level()
            } else {
                level
            };
            self.show_bar_osd(osd::Kind::Brightness, actual, false);
            Command::Brightness(level)
        };
        if let Some(worker) = &self.media.worker {
            let _ = worker.send(command);
        }
        self.guard(crate::module::Feature::Bar, (), |gv| gv.topbar.status = gv.media.status.clone());
        self.queue_redraw_all();
    }
    pub fn bar_release(&mut self, button: u32) -> () {
        self.guard(crate::module::Feature::Bar, (), |gv| gv.bar_release_feature(button))
    }
    fn bar_release_feature(&mut self, button: u32) -> () {
        if button == 0x110 {
            self.bar_drag(self.pointer_location());
            self.topbar.drag = None;
        }
    }
    pub fn bar_axis(&mut self, output: &Output, p: Point<f64, Logical>, delta: f64) -> bool {
        self.guard(crate::module::Feature::Bar, false, |gv| gv.bar_axis_feature(output, p, delta))
    }
    fn bar_axis_feature(&mut self, output: &Output, p: Point<f64, Logical>, delta: f64) -> bool {
        let action = self.topbar.action(output, p);
        if matches!(action, Some(BarAction::Volume | BarAction::Brightness)) {
            let action = action.unwrap();
            let old = if action == BarAction::Volume {
                self.media.status.volume.map(|v| v.level).unwrap_or(0.0)
            } else {
                self.media
                    .status
                    .brightness
                    .as_ref()
                    .map(|b| b.level())
                    .unwrap_or(0.0)
            };
            if delta != 0.0 {
                self.bar_set_slider(action, (old - delta.signum() * 0.05).clamp(0.0, 1.0));
            }
            return true;
        }
        let Some(v) = self.topbar.views.iter().find(|v| &v.output == output) else {
            return false;
        };
        let Some(l) = &v.layout else {
            return false;
        };
        if l.volume.to_f64().contains(p) {
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
        topbar::{BarView, Canvas, Layout, Popover, PopoverLayout},
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
        assert_eq!(h.gv.topbar.popover, Popover::Quick);
        h.gv.bar_action(&o, BarAction::Calendar);
        assert_eq!(h.gv.topbar.popover, Popover::Calendar);
        assert_eq!(h.gv.topbar.menu, Some(o.clone()));
        assert!(h.gv.bar_key(keysyms::KEY_Escape, 9, true));
        assert!(h.gv.topbar.menu.is_none());
        h.gv.bar_action(&o, BarAction::PowerMenu);
        assert!(h.gv.bar_key(keysyms::KEY_Super_L, 133, true));
        assert!(h.gv.topbar.menu.is_none());
        assert!(h.gv.bar_key(keysyms::KEY_Super_L, 133, false));
        assert!(!h.gv.media.core_keys.contains(&133));
    }
    #[test]
    fn slider_drag_release_keeps_the_final_value_and_never_leaks_to_clients() {
        let (mut h, o) = setup();
        let (tx, rx) = mpsc::channel();
        h.gv.media.worker = Some(tx);
        let panel = PopoverLayout::quick(1646, false);
        let slider = panel
            .items
            .iter()
            .find(|(i, _)| *i == super::super::Item::Volume)
            .unwrap()
            .1;
        h.gv.topbar.views.push(BarView {
            output: o.clone(),
            canvas: Canvas::default(),
            dots: vec![],
            hits: vec![(slider.to_f64(), BarAction::Volume)],
            visible: true,
            hover: None,
            layout: Some(Layout::new(1646, 55, true)),
            popover: Some(panel),
        });
        h.gv.bar_action(&o, BarAction::PowerMenu);
        h.gv.topbar.drag = Some((o, BarAction::Volume));
        h.gv.bar_drag(
            (
                slider.loc.x as f64 + slider.size.w as f64 - 7.0,
                slider.loc.y as f64 + 16.0,
            )
                .into(),
        );
        assert!(matches!(rx.try_recv(),Ok(Command::Volume(v)) if v==1.0));
        assert!(h.gv.shell_contains((800.0, 500.0).into()));
        h.gv.bar_release(0x110);
        assert!(h.gv.topbar.drag.is_none());
        assert!(matches!(rx.try_recv(),Ok(Command::Volume(v)) if v==0.0));
        // The current pointer is at the origin, so release clamps the final
        // drag to zero even though it is outside the popover.
        assert_eq!(h.gv.media.status.volume.unwrap().level, 0.0);
    }
}
