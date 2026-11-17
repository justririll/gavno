//! Perceptually scheduled palettes and damage-tracked sky elements. No private timer,
//! worker, image buffer or output-sized allocation is needed by this module.
use super::{
    shader::{self, Sky},
    spring::{Spring, SpringParams},
    theme::{self, LocalTime, Palette},
};
use crate::{
    config::{parse_color, ThemeConfig},
    module::Feature,
    render::GavnoRenderElement,
    topbar::{next_minute, wall_time},
    Gavno,
};
use smithay::{
    backend::renderer::{
        element::{
            solid::{SolidColorBuffer, SolidColorRenderElement},
            Element, Kind,
        },
        gles::{GlesPixelProgram, GlesRenderer, GlesTexture},
        ContextId, Renderer,
    },
    desktop::layer_map_for_output,
    output::Output,
    reexports::calloop::timer::{TimeoutAction, Timer},
    utils::{Logical, Rectangle, Size},
    wayland::shell::wlr_layer::Layer,
};
use std::time::Duration;

pub const FIXED_ACCENT: [f32; 4] = [0.95, 0.64, 0.42, 1.0];
/// Accumulate small changes until a visible transition is warranted.
pub const UPDATE_THRESHOLD: f64 = 0.02;
const JUMP_THRESHOLD: f64 = 0.08;
const LOOKAHEAD_MINUTES: u32 = 360;
const LATE: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Update {
    Clock,
    Config,
    Resume,
}

fn sample(time: &LocalTime, lat: f64, lon: f64, config: &ThemeConfig) -> Palette {
    let mut p = theme::palette(time, lat, lon);
    if config.background == "solid" {
        p = Palette::solid(config.solid(), [p.accent[0], p.accent[1], p.accent[2], 1.0]);
    }
    if let Some(c) = parse_color(&config.accent) {
        p.accent = [c[0], c[1], c[2]];
    }
    p
}

/// One-minute minimum spacing caps ordinary updates at 60/hour. Search the
/// actual solar trajectory; flat nights sleep for up to six hours, not one minute.
fn next_change(
    time: &LocalTime,
    lat: f64,
    lon: f64,
    config: &ThemeConfig,
    anchor: Palette,
) -> Duration {
    let minutes = (1..=LOOKAHEAD_MINUTES)
        .find(|m| {
            anchor.perceptual_delta(sample(&time.after_minutes(*m), lat, lon, config))
                >= UPDATE_THRESHOLD
        })
        .unwrap_or(LOOKAHEAD_MINUTES);
    Duration::from_secs(u64::from(minutes) * 60)
}

struct View {
    output: Output,
    sky: Sky,
    program: Option<GlesPixelProgram>,
    context: Option<ContextId<GlesTexture>>,
    solid: SolidColorBuffer,
    visible: Option<bool>,
    accent: Option<[f32; 4]>,
}
pub struct Background {
    pub initialized: bool,
    from: Option<Palette>,
    target: Option<Palette>,
    fade: Spring,
    views: Vec<View>,
    intro_pending: bool,
    suppress_animation: bool,
    next_due: Option<Duration>,
    planned_at: Duration,
    planned_wall: Duration,
    clock_due: Option<Duration>,
    bar_minute: Option<u64>,
}
impl Default for Background {
    fn default() -> Self {
        Self {
            initialized: false,
            from: None,
            target: None,
            fade: Spring::new(1.0, SpringParams { k: 36.0, zeta: 1.0 }),
            views: Vec::new(),
            intro_pending: true,
            suppress_animation: false,
            next_due: None,
            planned_at: Duration::ZERO,
            planned_wall: Duration::ZERO,
            clock_due: None,
            bar_minute: None,
        }
    }
}
impl Background {
    #[cfg(test)]
    pub fn update(&mut self, next: Palette, now: Duration) -> bool {
        self.change(next, now, Update::Clock)
    }
    fn change(&mut self, next: Palette, now: Duration, reason: Update) -> bool {
        self.initialized = true;
        if self.target.is_some_and(|p| {
            if reason == Update::Clock {
                p.perceptual_delta(next) < UPDATE_THRESHOLD
            } else {
                p == next
            }
        }) {
            return false;
        }
        let previous = self.current(now).unwrap_or(next);
        let fade = reason != Update::Clock && previous.perceptual_delta(next) >= JUMP_THRESHOLD;
        self.from = Some(if fade { previous } else { next });
        self.target = Some(next);
        self.fade.jump(if fade { 0.0 } else { 1.0 });
        self.fade.advance(now);
        self.fade.set(1.0);
        true
    }
    fn prepare_intro(&mut self, config: &ThemeConfig, now: Duration) {
        if !self.intro_pending || self.target.is_none() {
            return;
        }
        self.intro_pending = false;
        let start = Palette::solid(config.solid(), FIXED_ACCENT);
        if start.perceptual_delta(self.target.unwrap()) >= JUMP_THRESHOLD {
            self.from = Some(start);
            self.fade.jump(0.0);
            self.fade.advance(now);
            self.fade.set(1.0);
        }
    }
    fn plan(
        &mut self,
        time: &LocalTime,
        lat: f64,
        lon: f64,
        config: &ThemeConfig,
        now: Duration,
        wall: Duration,
    ) {
        self.next_due = self
            .target
            .map(|p| now + next_change(time, lat, lon, config, p));
        self.planned_at = now;
        self.planned_wall = wall;
    }
    fn clock_jump(&self, now: Duration, wall: Duration) -> bool {
        wall.saturating_sub(self.planned_wall)
            .abs_diff(now.saturating_sub(self.planned_at))
            > LATE
            || self
                .next_due
                .is_some_and(|due| now.saturating_sub(due) > LATE)
    }
    fn due(&self, now: Duration, wall: Duration) -> bool {
        self.next_due.is_none_or(|due| now >= due) || self.clock_jump(now, wall)
    }
    pub fn visible(&self, output: &Output) -> bool {
        self.views
            .iter()
            .find(|v| &v.output == output)
            .and_then(|v| v.visible)
            .unwrap_or(true)
    }
    /// Authoritative coverage comes from the same opaque regions Smithay uses
    /// for damage, including rounded/transformed windows and opaque layer clients.
    pub fn record_visibility(
        &mut self,
        output: &Output,
        foreground: &[GavnoRenderElement],
        size: Size<i32, Logical>,
        scale: f64,
    ) {
        let physical = output
            .current_mode()
            .map(|m| output.current_transform().transform_size(m.size))
            .unwrap_or_else(|| size.to_physical_precise_round(scale));
        let area = Rectangle::from_size(physical);
        let opaque = foreground.iter().flat_map(|e| {
            let loc = e.geometry(scale.into()).loc;
            e.opaque_regions(scale.into())
                .into_iter()
                .map(move |r| Rectangle::new(r.loc + loc, r.size))
        });
        self.view(output).visible = Some(!area.subtract_rects(opaque).is_empty());
    }
    pub fn accent_for(&mut self, output: &Output, config: &ThemeConfig, now: Duration) -> [f32; 4] {
        self.prepare_intro(config, now);
        let accent = self.accent(config, now);
        let view = self.view(output);
        if view.visible == Some(false) {
            return view.accent.unwrap_or(accent);
        }
        view.accent = Some(accent);
        accent
    }
    pub fn active(&self) -> bool {
        !self.suppress_animation && self.fade.is_active()
    }
    pub fn current(&mut self, now: Duration) -> Option<Palette> {
        let f = self.fade.advance(now) as f32;
        if f == 1.0 {
            self.target
        } else {
            Some(self.from?.mix(self.target?, f))
        }
    }
    pub fn accent(&mut self, _config: &ThemeConfig, now: Duration) -> [f32; 4] {
        self.current(now)
            .map(|p| [p.accent[0], p.accent[1], p.accent[2], 1.0])
            .unwrap_or(FIXED_ACCENT)
    }
    fn view(&mut self, output: &Output) -> &mut View {
        let index = self
            .views
            .iter()
            .position(|v| &v.output == output)
            .unwrap_or_else(|| {
                self.views.push(View {
                    output: output.clone(),
                    sky: Sky::default(),
                    program: None,
                    context: None,
                    solid: SolidColorBuffer::default(),
                    visible: None,
                    accent: None,
                });
                self.views.len() - 1
            });
        &mut self.views[index]
    }
    pub fn retain_outputs(&mut self, outputs: &[Output]) {
        self.views.retain(|v| outputs.contains(&v.output));
    }
    pub fn solid(
        &mut self,
        output: &Output,
        size: Size<i32, Logical>,
        color: [f32; 4],
    ) -> GavnoRenderElement {
        let view = self.view(output);
        view.solid.resize(size);
        view.solid.set_color(color);
        GavnoRenderElement::Solid(SolidColorRenderElement::from_buffer(
            &view.solid,
            (0, 0),
            output.current_scale().fractional_scale(),
            1.0,
            Kind::Unspecified,
        ))
    }
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        config: &ThemeConfig,
        now: Duration,
    ) -> Option<GavnoRenderElement> {
        self.prepare_intro(config, now);
        if config.background != "living" && !self.active() {
            return None;
        }
        let pal = self.current(now)?;
        let view = self.view(output);
        if view.context.as_ref() != Some(&renderer.context_id()) {
            view.sky = Sky::default();
            view.program = Some(shader::compile_background(renderer).expect("compile living sky"));
            view.context = Some(renderer.context_id());
        }
        Some(GavnoRenderElement::Shader(view.sky.element(
            view.program.as_ref().unwrap(),
            Rectangle::from_size(size),
            1.0,
            scale,
            pal,
        )))
    }
}

/// Mapped background-layer surfaces whose arranged geometry covers the output
/// are wallpaper providers. Partial panels and bottom-layer widgets are not.
pub fn has_wallpaper(output: &Output, size: Size<i32, Logical>) -> bool {
    let layers = layer_map_for_output(output);
    let covered = layers.layers().any(|layer| {
        wallpaper_covers(layer.layer(), layers.layer_geometry(layer), size)
            && smithay::backend::renderer::utils::with_renderer_surface_state(
                layer.wl_surface(),
                |state| state.buffer().is_some(),
            )
            .unwrap_or(false)
    });
    covered
}
fn wallpaper_covers(
    layer: Layer,
    geometry: Option<Rectangle<i32, Logical>>,
    size: Size<i32, Logical>,
) -> bool {
    layer == Layer::Background
        && geometry.is_some_and(|g| g.contains_rect(Rectangle::from_size(size)))
}

impl Gavno {
    /// Limit only the theme's contribution; core animation gates (including
    /// backend power gates) remain in the shared is_animating implementation.
    pub(crate) fn is_animating_for(&mut self, output: &Output) -> bool {
        let previous = self.background.suppress_animation;
        self.background.suppress_animation = !self.theme_output_visible(output);
        let active = self.is_animating();
        self.background.suppress_animation = previous;
        active
    }
    pub(crate) fn update_theme(&mut self) {
        self.sample_theme(Update::Config);
    }
    pub(crate) fn resume_theme(&mut self) {
        self.sample_theme(Update::Resume);
    }
    fn tick_theme(&mut self) {
        let now = self.start_time.elapsed();
        let wall = wall_time();
        if !self.background.due(now, wall) {
            return;
        }
        let reason = if self.background.clock_jump(now, wall) {
            Update::Resume
        } else {
            Update::Clock
        };
        self.sample_theme(reason);
    }
    fn sample_theme(&mut self, reason: Update) {
        let now = self.start_time.elapsed();
        let changed = self.guard(Feature::Theme, false, |gv| {
            let time = LocalTime::now();
            let (lat, lon) = gv.config.theme.location(time.utc_offset);
            let changed =
                gv.background
                    .change(sample(&time, lat, lon, &gv.config.theme), now, reason);
            gv.background
                .plan(&time, lat, lon, &gv.config.theme, now, wall_time());
            changed
        });
        self.sync_theme_accent(now);
        if changed {
            self.queue_theme_redraws();
        }
    }
    pub(crate) fn sync_theme_accent(&mut self, now: Duration) {
        let accent = self.guard(Feature::Theme, FIXED_ACCENT, |gv| {
            gv.background.accent(&gv.config.theme, now)
        });
        self.topbar.set_accent(accent);
        if let Some(portal) = &mut self.portal {
            portal.accent(accent);
        }
    }
    pub(crate) fn theme_output_visible(&self, output: &Output) -> bool {
        !self.protocols.locks.locked && self.greeter.is_none() && self.background.visible(output)
    }
    pub(crate) fn theme_visible(&self) -> bool {
        self.space.outputs().any(|o| {
            self.theme_output_visible(o)
                && (self.background.active()
                    || self.config.theme.accent == "living"
                    || (self.config.theme.background == "living"
                        && self
                            .space
                            .output_geometry(o)
                            .is_some_and(|g| !has_wallpaper(o, g.size))))
        })
    }
    pub(crate) fn theme_redraw_outputs(&self) -> Vec<Output> {
        self.space
            .outputs()
            .filter(|o| self.theme_output_visible(o))
            .cloned()
            .collect()
    }
    pub(crate) fn queue_theme_redraws(&mut self) {
        for output in self.theme_redraw_outputs() {
            crate::backend::queue_redraw_output(self, &output);
        }
    }
    fn bar_clock_needed(&self) -> bool {
        // Nothing shows a clock while the outputs are powered off.
        !self.power.is_off()
            && !self.protocols.locks.locked
            && self.greeter.is_none()
            && self.features.on(Feature::Bar)
            && self.config.bar.enabled
            && self.space.outputs().any(|o| self.shell_visible(o))
    }
    fn refresh_bar_clock(&mut self) {
        if !self.bar_clock_needed() {
            return;
        }
        let minute = wall_time().as_secs() / 60;
        if self.background.bar_minute == Some(minute) {
            return;
        }
        self.background.bar_minute = Some(minute);
        let changed = self.guard(Feature::Bar, false, |gv| gv.topbar.update_clock());
        if changed {
            let outputs: Vec<_> = self
                .space
                .outputs()
                .filter(|o| self.shell_visible(o))
                .cloned()
                .collect();
            for output in outputs {
                crate::backend::queue_redraw_output(self, &output);
            }
        }
    }
    /// One shared one-shot clock: the next visible theme change, or the next
    /// minute if a panel clock is visible. No minute palette sampling when settled.
    pub(crate) fn refresh_theme_clock(&mut self) {
        if !self.background.initialized && self.features.on(Feature::Theme) {
            self.update_theme();
        }
        let now = self.start_time.elapsed();
        let theme_needed =
            !self.power.is_off() && self.features.on(Feature::Theme) && self.theme_visible();
        if theme_needed {
            self.tick_theme();
        }
        self.sync_theme_accent(now);
        self.refresh_bar_clock();
        let theme_due = theme_needed.then_some(self.background.next_due).flatten();
        let bar_due = self
            .bar_clock_needed()
            .then(|| now + next_minute(wall_time()));
        // A visible panel already wakes each minute: service theme deadlines
        // there rather than adding a second wake between two clock ticks.
        let due = bar_due.or(theme_due);
        if self
            .background
            .clock_due
            .zip(due)
            .is_some_and(|(old, new)| old.abs_diff(new) < Duration::from_secs(1))
            && self.minute_timer.is_some()
        {
            return;
        }
        if let Some(token) = self.minute_timer.take() {
            self.loop_handle.remove(token);
        }
        self.background.clock_due = due;
        let Some(due) = due else {
            return;
        };
        match self.loop_handle.insert_source(
            Timer::from_duration(due.saturating_sub(now)),
            |_, _, gv| {
                gv.minute_timer = None;
                gv.background.clock_due = None;
                gv.refresh_theme_clock();
                TimeoutAction::Drop
            },
        ) {
            Ok(token) => self.minute_timer = Some(token),
            Err(error) => tracing::warn!(%error, "cannot schedule shell clock"),
        }
    }
    pub(crate) fn reset_theme(&mut self) {
        self.background = Background::default();
        self.background.initialized = true;
        self.topbar.set_accent(FIXED_ACCENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::{
        backend::{
            egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
            renderer::element::Element,
        },
        output::{Mode, PhysicalProperties},
        utils::Transform,
    };

    fn time(day: u32, minute: u32) -> LocalTime {
        LocalTime {
            year: 2026,
            month: 6,
            day: 21,
            weekday: 0,
            hour: minute / 60,
            minute: minute % 60,
            second: 0,
            yday: day,
            utc_offset: 10800,
        }
    }
    fn output() -> Output {
        let o = Output::new(
            "theme-test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        o.change_current_state(
            Some(Mode {
                size: (960, 540).into(),
                refresh: 60000,
            }),
            Some(Transform::Normal),
            None,
            None,
        );
        o
    }
    fn renderer() -> GlesRenderer {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        unsafe { GlesRenderer::new(EGLContext::new(&display).unwrap()) }.unwrap()
    }
    #[test]
    fn invisible_changes_are_skipped_and_ordinary_updates_never_fade() {
        let mut b = Background::default();
        let p = theme::palette(&time(319, 0), 55.75, 37.62);
        assert!(b.update(p, Duration::ZERO));
        assert!(!b.active());
        assert!(!b.update(p, Duration::from_secs(60)));
        let mut nearly = p;
        nearly.sky_top[0] += 0.001;
        assert!(p.perceptual_delta(nearly) < UPDATE_THRESHOLD);
        assert!(!b.update(nearly, Duration::from_secs(120)));
        assert_eq!(b.current(Duration::from_secs(120)), Some(p));
        nearly.sky_top[0] += 0.1;
        assert!(p.perceptual_delta(nearly) >= UPDATE_THRESHOLD);
        assert!(b.update(nearly, Duration::from_secs(180)));
        assert!(
            !b.active(),
            "ordinary solar updates must not request a second frame"
        );
        assert_eq!(b.current(Duration::from_secs(180)), Some(nearly));
        // Even a large ordinary delta never silently opts into a fade.
        let noon = theme::palette(&time(172, 720), 55.75, 37.62);
        assert!(b.update(noon, Duration::from_secs(240)));
        assert!(!b.active());
    }

    #[test]
    fn only_explicit_large_jumps_and_startup_fade() {
        for reason in [Update::Config, Update::Resume] {
            let mut b = Background::default();
            let night = theme::palette(&time(319, 0), 55.75, 37.62);
            b.update(night, Duration::ZERO);
            let mut small = night;
            small.sky_top[0] += 0.001;
            assert!(b.change(small, Duration::from_secs(1), reason));
            assert!(!b.active());
            let noon = theme::palette(&time(172, 720), 55.75, 37.62);
            assert!(b.change(noon, Duration::from_secs(2), reason));
            assert!(b.active());
            assert_eq!(b.current(Duration::from_secs(2)), Some(small));
            for frame in 1..=300 {
                b.current(Duration::from_secs(2) + Duration::from_micros(frame * 8_334));
            }
            assert_eq!(b.current(Duration::from_secs(5)), Some(noon));
            assert!(!b.active());
        }
        let mut b = Background::default();
        b.update(
            theme::palette(&time(172, 720), 55.75, 37.62),
            Duration::ZERO,
        );
        b.prepare_intro(&ThemeConfig::default(), Duration::ZERO);
        assert!(
            b.active(),
            "the first visible frame may fade from the configured solid color"
        );
    }

    #[test]
    fn solid_fixed_theme_animates_a_large_edit_but_has_no_steady_clock() {
        let mut h = crate::state::tests::Harness::new();
        let output = output();
        h.gv.space.map_output(&output, (0, 0));
        h.gv.config.modules.bar = false;
        h.gv.config.modules.dock = false;
        h.gv.reload_features();
        h.gv.config.theme.background = "solid".into();
        h.gv.config.theme.accent = "#123456".into();
        h.gv.background.intro_pending = false;
        let old = Palette::solid([0.05, 0.05, 0.05, 1.0], parse_color("#123456").unwrap());
        let next = Palette::solid([0.9, 0.9, 0.9, 1.0], parse_color("#123456").unwrap());
        h.gv.background.update(old, Duration::ZERO);
        h.gv.background.change(next, Duration::ZERO, Update::Config);
        assert!(
            h.gv.is_animating_for(&output),
            "explicit edits must fade even when both choices are fixed"
        );
        for frame in 1..=300 {
            h.gv.background
                .current(Duration::from_micros(frame * 8_334));
        }
        assert!(!h.gv.is_animating_for(&output));
        h.gv.refresh_theme_clock();
        assert!(
            h.gv.minute_timer.is_none(),
            "a settled fixed theme has no clock source"
        );
    }

    #[test]
    fn threshold_scheduler_skips_flat_minutes_and_detects_late_resume() {
        let config = ThemeConfig::default();
        for t in [time(319, 0), time(172, 720), time(172, 240)] {
            let anchor = sample(&t, 55.75, 37.62, &config);
            let delay = next_change(&t, 55.75, 37.62, &config, anchor);
            let minutes = delay.as_secs() as u32 / 60;
            assert!((1..=LOOKAHEAD_MINUTES).contains(&minutes));
            for minute in 1..minutes {
                assert!(
                    anchor.perceptual_delta(sample(
                        &t.after_minutes(minute),
                        55.75,
                        37.62,
                        &config
                    )) < UPDATE_THRESHOLD
                );
            }
            if minutes < LOOKAHEAD_MINUTES {
                assert!(
                    anchor.perceptual_delta(sample(
                        &t.after_minutes(minutes),
                        55.75,
                        37.62,
                        &config
                    )) >= UPDATE_THRESHOLD
                );
            }
            let mut b = Background::default();
            b.update(anchor, Duration::ZERO);
            b.plan(&t, 55.75, 37.62, &config, Duration::ZERO, Duration::ZERO);
            assert!(!b.due(
                delay - Duration::from_secs(1),
                delay - Duration::from_secs(1)
            ));
            assert!(b.due(delay, delay));
            assert!(
                !b.clock_jump(delay, delay),
                "a deliberately long night sleep is not a missed wakeup"
            );
            assert!(b.clock_jump(
                delay + LATE + Duration::from_secs(1),
                delay + LATE + Duration::from_secs(1)
            ));
            assert!(
                b.clock_jump(Duration::from_secs(1), Duration::from_secs(3600)),
                "suspend can advance wall time without MONOTONIC"
            );
        }
        let night = time(319, 0);
        assert_eq!(
            next_change(
                &night,
                55.75,
                37.62,
                &config,
                sample(&night, 55.75, 37.62, &config)
            ),
            Duration::from_secs(6 * 3600)
        );
        let noon = time(172, 720);
        assert!(
            next_change(
                &noon,
                55.75,
                37.62,
                &config,
                sample(&noon, 55.75, 37.62, &config)
            ) >= Duration::from_secs(600)
        );
    }

    #[test]
    fn wallpaper_requires_full_background_layer_coverage() {
        let size = (960, 540).into();
        assert!(wallpaper_covers(
            Layer::Background,
            Some(Rectangle::from_size(size)),
            size
        ));
        assert!(wallpaper_covers(
            Layer::Background,
            Some(Rectangle::new((-2, -2).into(), (964, 544).into())),
            size
        ));
        assert!(!wallpaper_covers(
            Layer::Bottom,
            Some(Rectangle::from_size(size)),
            size
        ));
        assert!(!wallpaper_covers(
            Layer::Background,
            Some(Rectangle::from_size((959, 540).into())),
            size
        ));
        assert!(!wallpaper_covers(Layer::Background, None, size));
    }
    #[test]
    fn feature_disable_reload_and_panic_use_solid_and_fixed_accent() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.update_theme();
        assert!(h.gv.features.on(Feature::Theme));
        h.gv.features.inject_panic(Feature::Theme);
        h.gv.update_theme();
        assert!(!h.gv.features.on(Feature::Theme));
        assert_eq!(h.gv.topbar.accent(), FIXED_ACCENT);
        assert!(!h.gv.background.active());
        h.gv.reload_features();
        assert!(
            !h.gv.features.on(Feature::Theme),
            "crashed themes require restart"
        );
        let mut h = crate::state::tests::Harness::new();
        h.gv.config.modules.theme = false;
        h.gv.reload_features();
        h.gv.update_theme();
        assert_eq!(h.gv.topbar.accent(), FIXED_ACCENT);
        h.gv.config.modules.theme = true;
        h.gv.reload_features();
        h.gv.update_theme();
        assert!(h.gv.features.on(Feature::Theme));
        assert!(h.gv.background.initialized);
        h.gv.config.theme.accent = "#123456".into();
        h.gv.update_theme();
        h.gv.background.fade.jump(1.0);
        h.gv.sync_theme_accent(h.gv.start_time.elapsed());
        assert_eq!(h.gv.topbar.accent(), parse_color("#123456").unwrap());
    }
    #[test]
    fn theme_reload_and_shared_clock_work_without_the_bar() {
        let mut h = crate::state::tests::Harness::new();
        let o = output();
        h.gv.space.map_output(&o, (0, 0));
        h.gv.config.modules.bar = false;
        h.gv.reload_features();
        h.gv.refresh_shell();
        let token = h.gv.minute_timer;
        assert!(token.is_some());
        h.gv.refresh_shell();
        assert_eq!(
            h.gv.minute_timer, token,
            "refresh must reuse the minute source"
        );
        let next = crate::config::Config::parse_checked(true, "[theme]\nbackground='solid'\nsolid_color='#223344'\naccent='#abcdef'\nlatitude=-33.87\nlongitude=151.21\n[modules]\nbar=false", |_| None).unwrap();
        let prepared = crate::config_reload::Prepared::new(next, &h.gv.config).unwrap();
        crate::config_reload::apply(&mut h.gv, prepared).unwrap();
        h.gv.background.fade.jump(1.0);
        h.gv.sync_theme_accent(h.gv.start_time.elapsed());
        assert_eq!(h.gv.topbar.accent(), parse_color("#abcdef").unwrap());
        assert_eq!(h.gv.config.theme.location(0), (-33.87, 151.21));
        assert_eq!(h.gv.config.theme.solid(), parse_color("#223344").unwrap());
        h.gv.config.modules.theme = false;
        h.gv.reload_features();
        h.gv.refresh_shell();
        assert!(h.gv.minute_timer.is_none());
        assert!(!h.gv.is_animating());
    }

    #[test]
    #[ignore = "isolated Linux RSS measurement"]
    fn measure_theme_memory() {
        let mut r = renderer();
        let o = output();
        // Compile once before sampling to exclude Mesa's first-use compiler/JIT.
        let program = shader::compile_background(&mut r).unwrap();
        std::hint::black_box(&program);
        let before = crate::classic::cache::performance::rss_kib();
        let mut b = Background::default();
        b.intro_pending = false;
        b.update(
            theme::palette(&time(172, 720), 55.75, 37.62),
            Duration::ZERO,
        );
        for minute in 0..60 {
            let e = b
                .elements(
                    &mut r,
                    &o,
                    (3840, 2160).into(),
                    1.0,
                    &ThemeConfig::default(),
                    Duration::from_secs(minute * 60),
                )
                .unwrap();
            std::hint::black_box(e);
        }
        r.with_context(|gl| unsafe { gl.Finish() }).unwrap();
        let after = crate::classic::cache::performance::rss_kib();
        eprintln!("theme memory: Background={} bytes; View={} bytes; Vec capacity={}; retained RSS delta={} KiB; output pixel buffers=0", std::mem::size_of::<Background>(), std::mem::size_of::<View>(), b.views.capacity(), after.saturating_sub(before));
    }

    #[test]
    fn cached_sky_has_stable_identity_commit_and_no_idle_damage() {
        use smithay::backend::{
            allocator::Fourcc,
            renderer::{damage::OutputDamageTracker, gles::GlesTexture, Bind, Offscreen},
        };
        let mut r = renderer();
        let o = output();
        let mut b = Background::default();
        b.intro_pending = false;
        let config = ThemeConfig::default();
        b.update(
            theme::palette(&time(172, 720), 55.75, 37.62),
            Duration::ZERO,
        );
        let e = b
            .elements(&mut r, &o, (960, 540).into(), 1.0, &config, Duration::ZERO)
            .unwrap();
        let again = b
            .elements(
                &mut r,
                &o,
                (960, 540).into(),
                1.0,
                &config,
                Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(e.id(), again.id());
        assert_eq!(e.current_commit(), again.current_commit());
        let id = e.id().clone();
        let commit = e.current_commit();
        let mut accent_only = b.target.unwrap();
        accent_only.accent = [1.0, 0.0, 0.0];
        assert!(b.update(accent_only, Duration::from_secs(2)));
        let accent_sky = b
            .elements(
                &mut r,
                &o,
                (960, 540).into(),
                1.0,
                &config,
                Duration::from_secs(2),
            )
            .unwrap();
        assert_eq!(
            accent_sky.current_commit(),
            commit,
            "accent-only updates cannot damage the sky"
        );
        let mut texture: GlesTexture = r
            .create_buffer(Fourcc::Abgr8888, (960, 540).into())
            .unwrap();
        let mut tracker = OutputDamageTracker::new((960, 540), 1.0, Transform::Normal);
        let mut fb = r.bind(&mut texture).unwrap();
        assert!(tracker
            .render_output(&mut r, &mut fb, 0, &[e], crate::render::CLEAR_COLOR)
            .unwrap()
            .damage
            .is_some());
        assert!(tracker
            .render_output(&mut r, &mut fb, 1, &[again], crate::render::CLEAR_COLOR)
            .unwrap()
            .damage
            .is_none());
        b.update(
            theme::palette(&time(172, 1080), 55.75, 37.62),
            Duration::from_secs(60),
        );
        for f in 1..=150 {
            b.current(Duration::from_secs(60) + Duration::from_micros(f * 16_667));
        }
        let changed = b
            .elements(
                &mut r,
                &o,
                (960, 540).into(),
                1.0,
                &config,
                Duration::from_secs(63),
            )
            .unwrap();
        assert_eq!(changed.id(), &id);
        assert_ne!(changed.current_commit(), commit);
        let mut replacement = renderer();
        let recreated = b
            .elements(
                &mut replacement,
                &o,
                (960, 540).into(),
                1.0,
                &config,
                Duration::from_secs(63),
            )
            .unwrap();
        assert_ne!(
            recreated.id(),
            &id,
            "renderer replacement must rebuild its GL program"
        );
    }
    fn desktop_scene(
        r: &mut GlesRenderer,
        gv: &mut Gavno,
        output: &Output,
        now: Duration,
    ) -> Vec<GavnoRenderElement> {
        crate::render::output_elements(
            r,
            &gv.space,
            output,
            (-1.0, -1.0).into(),
            &mut gv.cursor_status,
            &mut gv.cursor,
            now,
            None,
            &mut gv.lock,
            &mut gv.overlay,
            &mut gv.window_decor,
            &mut gv.window_animations,
            &gv.protocols.locks,
            &mut gv.workspaces,
            &mut gv.overview,
            &mut gv.dock,
            &mut gv.topbar,
            &gv.config,
            &mut gv.features,
            &mut gv.media.osd,
            &mut gv.launcher,
            &mut gv.classic,
            &mut gv.polkit,
            &mut gv.background,
        )
    }

    #[test]
    fn opaque_client_suppresses_theme_frames_only_on_its_output() {
        use smithay::backend::renderer::damage::OutputDamageTracker;
        let mut h = crate::state::tests::Harness::new();
        h.gv.config.modules.decorations = false;
        h.gv.config.modules.animations = false;
        h.gv.config.modules.dock = false;
        h.gv.config.modules.bar = false;
        h.gv.reload_features();
        let covered = output();
        let exposed = output();
        h.gv.space.map_output(&covered, (0, 0));
        h.gv.space.map_output(&exposed, (960, 0));
        let window = h.window();
        h.gv.map_new_window(window.clone());
        h.commit_size(&window, 960, 540);
        h.gv.space.map_element(window.clone(), (0, 0), false);
        h.gv.background.intro_pending = false;
        h.gv.background.update(
            theme::palette(&time(172, 720), 55.75, 37.62),
            Duration::ZERO,
        );
        let mut r = renderer();
        let mut tracker = OutputDamageTracker::new((960, 540), 1.0, Transform::Normal);
        let first = desktop_scene(&mut r, &mut h.gv, &covered, Duration::ZERO);
        tracker.damage_output(0, &first).unwrap();
        desktop_scene(&mut r, &mut h.gv, &exposed, Duration::ZERO);
        assert!(!h.gv.background.visible(&covered));
        assert!(h.gv.background.visible(&exposed));
        assert_eq!(h.gv.theme_redraw_outputs(), vec![exposed.clone()]);
        let old_accent =
            h.gv.background
                .accent_for(&covered, &h.gv.config.theme, Duration::ZERO);
        h.gv.background.update(
            theme::palette(&time(319, 0), 55.75, 37.62),
            Duration::from_secs(60),
        );
        assert!(!h.gv.background.active());
        let next = desktop_scene(&mut r, &mut h.gv, &covered, Duration::from_secs(60));
        assert!(
            tracker.damage_output(1, &next).unwrap().0.is_none(),
            "a hidden background update must not damage clients or chrome"
        );
        assert_eq!(
            h.gv.topbar.accent(),
            old_accent,
            "covered outputs retain their accent until exposed"
        );
        assert!(!h.gv.is_animating_for(&covered));
        h.gv.background.change(
            theme::palette(&time(172, 720), 55.75, 37.62),
            Duration::from_secs(61),
            Update::Config,
        );
        assert!(h.gv.background.active());
        assert!(
            !h.gv.is_animating_for(&covered),
            "another output's exceptional fade cannot wake an occluded output"
        );
        assert!(h.gv.is_animating_for(&exposed));
        h.gv.space.unmap_elem(&window);
        desktop_scene(&mut r, &mut h.gv, &covered, Duration::from_secs(62));
        assert!(h.gv.background.visible(&covered));
        assert!(h.gv.theme_redraw_outputs().contains(&covered));
    }

    #[test]
    fn opaque_coverage_handles_unions_holes_alpha_and_fractional_scale() {
        let o = output();
        let mut b = Background::default();
        let left = SolidColorBuffer::new((480, 540), [0.1, 0.1, 0.1, 1.0]);
        let right = SolidColorBuffer::new((480, 540), [0.2, 0.2, 0.2, 1.0]);
        let element = |buffer: &SolidColorBuffer, x, alpha| {
            GavnoRenderElement::Solid(SolidColorRenderElement::from_buffer(
                buffer,
                (x, 0),
                1.0,
                alpha,
                Kind::Unspecified,
            ))
        };
        b.record_visibility(
            &o,
            &[element(&left, 0, 1.0), element(&right, 480, 1.0)],
            (960, 540).into(),
            1.0,
        );
        assert!(!b.visible(&o));
        b.record_visibility(
            &o,
            &[element(&left, 0, 1.0), element(&right, 481, 1.0)],
            (960, 540).into(),
            1.0,
        );
        assert!(b.visible(&o), "a one-pixel gap exposes the sky");
        b.record_visibility(
            &o,
            &[element(&left, 0, 0.5), element(&right, 480, 1.0)],
            (960, 540).into(),
            1.0,
        );
        assert!(b.visible(&o), "translucent clients cannot hide the sky");
        o.change_current_state(
            Some(Mode {
                size: (2880, 1620).into(),
                refresh: 120000,
            }),
            None,
            Some(smithay::output::Scale::Fractional(1.75)),
            None,
        );
        let full = SolidColorBuffer::new((1646, 926), [0.1, 0.1, 0.1, 1.0]);
        let full = GavnoRenderElement::Solid(SolidColorRenderElement::from_buffer(
            &full,
            (0, 0),
            1.75,
            1.0,
            Kind::Unspecified,
        ));
        b.record_visibility(&o, &[full], (1646, 926).into(), 1.75);
        assert!(
            !b.visible(&o),
            "logical rounding must not invent an exposed pixel beyond the physical panel"
        );
    }

    #[test]
    fn covered_2880_hdr_output_has_no_composition_or_pq_damage_at_120hz() {
        use smithay::backend::{
            allocator::Fourcc,
            renderer::{damage::OutputDamageTracker, Bind, Offscreen},
        };
        let mut r = renderer();
        let o = output();
        o.change_current_state(
            Some(Mode {
                size: (2880, 1620).into(),
                refresh: 120000,
            }),
            None,
            None,
            None,
        );
        let mut hdr = crate::hdr::compose::Composition::new(&mut r, &o, 203.0).unwrap();
        let mut scanout = OutputDamageTracker::new((2880, 1620), 1.0, Transform::Normal);
        let mut target: GlesTexture = r
            .create_buffer(Fourcc::Abgr2101010, (2880, 1620).into())
            .unwrap();
        let client = SolidColorBuffer::new((2880, 1620), [0.1, 0.12, 0.15, 1.0]);
        let client_element = || {
            GavnoRenderElement::Solid(SolidColorRenderElement::from_buffer(
                &client,
                (0, 0),
                1.0,
                1.0,
                Kind::Unspecified,
            ))
        };
        let mut b = Background::default();
        b.intro_pending = false;
        b.update(
            theme::palette(&time(172, 720), 55.75, 37.62),
            Duration::ZERO,
        );
        let config = ThemeConfig::default();
        let sky = b
            .elements(
                &mut r,
                &o,
                (2880, 1620).into(),
                1.0,
                &config,
                Duration::ZERO,
            )
            .unwrap();
        let (pq, _) = hdr
            .render(
                &mut r,
                &o,
                &[client_element(), sky],
                crate::render::CLEAR_COLOR,
            )
            .unwrap();
        assert!(hdr.needs_scanout());
        let commit = pq.current_commit();
        let mut fb = r.bind(&mut target).unwrap();
        assert!(scanout
            .render_output(&mut r, &mut fb, 0, &[pq], [0.0, 0.0, 0.0, 1.0])
            .unwrap()
            .damage
            .is_some());
        drop(fb);
        hdr.presented();
        b.update(
            theme::palette(&time(319, 0), 55.75, 37.62),
            Duration::from_secs(60),
        );
        let sky = b
            .elements(
                &mut r,
                &o,
                (2880, 1620).into(),
                1.0,
                &config,
                Duration::from_secs(60),
            )
            .unwrap();
        let sky = crate::classic::cache::CachedElement::from_ui(sky).unwrap();
        let (pq, _) = hdr
            .render(
                &mut r,
                &o,
                &[client_element(), GavnoRenderElement::Desktop(sky.clone())],
                crate::render::CLEAR_COLOR,
            )
            .unwrap();
        assert!(
            !hdr.needs_scanout(),
            "udev must skip HDR scanout entirely for this result"
        );
        assert_eq!(pq.current_commit(), commit);
        let mut fb = r.bind(&mut target).unwrap();
        assert!(scanout
            .render_output(&mut r, &mut fb, 1, &[pq], [0.0, 0.0, 0.0, 1.0])
            .unwrap()
            .damage
            .is_none());
        drop(fb);
        // An unpresented frame must remain eligible for a retry even if its
        // source texture is unchanged. Only a successful vblank clears it.
        let exposed = [GavnoRenderElement::Desktop(sky.clone())];
        hdr.render(&mut r, &o, &exposed, crate::render::CLEAR_COLOR)
            .unwrap();
        assert!(hdr.needs_scanout());
        hdr.render(&mut r, &o, &exposed, crate::render::CLEAR_COLOR)
            .unwrap();
        assert!(hdr.needs_scanout());
        hdr.presented();
        hdr.render(&mut r, &o, &exposed, crate::render::CLEAR_COLOR)
            .unwrap();
        assert!(!hdr.needs_scanout());
        // Exposure, connector reset/resume and white changes must still render.
        hdr.invalidate();
        hdr.render(&mut r, &o, &exposed, crate::render::CLEAR_COLOR)
            .unwrap();
        assert!(hdr.needs_scanout());
        hdr.presented();
        hdr.adjust_white(10.0);
        hdr.render(&mut r, &o, &exposed, crate::render::CLEAR_COLOR)
            .unwrap();
        assert!(hdr.needs_scanout());
    }

    /// Deterministic solar deadlines and actual Smithay damage, independent of
    /// wall-clock date, compositor traffic and monitor refresh. Opt-in artifacts.
    #[test]
    fn measure_theme_day_and_render_samples() {
        let mut r = renderer();
        let o = output();
        let config = ThemeConfig::default();
        let started = std::time::Instant::now();
        for (name, day) in [("june", 172), ("november", 319)] {
            let mut b = Background::default();
            b.intro_pending = false;
            let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
                (960, 540),
                1.0,
                Transform::Normal,
            );
            let t = time(day, 0);
            b.update(sample(&t, 55.75, 37.62, &config), Duration::ZERO);
            b.plan(&t, 55.75, 37.62, &config, Duration::ZERO, Duration::ZERO);
            let first = b
                .elements(&mut r, &o, (960, 540).into(), 1.0, &config, Duration::ZERO)
                .unwrap();
            tracker.damage_output(0, &[first]).unwrap();
            let mut updates = [0_u32; 24];
            let mut frames = [0_u32; 24];
            let mut samples = 0;
            for minute in 1..1440 {
                let now = Duration::from_secs(minute as u64 * 60);
                if !b.due(now, now) {
                    continue;
                }
                samples += 1;
                let t = time(day, minute);
                if b.update(sample(&t, 55.75, 37.62, &config), now) {
                    updates[minute as usize / 60] += 1;
                    assert!(
                        !b.active(),
                        "routine updates must take one frame at 60Hz or 120Hz"
                    );
                    let e = b
                        .elements(&mut r, &o, (960, 540).into(), 1.0, &config, now)
                        .unwrap();
                    if let Some(damage) = tracker.damage_output(1, &[e]).unwrap().0 {
                        assert_eq!(
                            damage.as_slice(),
                            &[Rectangle::from_size((960, 540).into())]
                        );
                        frames[minute as usize / 60] += 1;
                    }
                    let settled = b
                        .elements(
                            &mut r,
                            &o,
                            (960, 540).into(),
                            1.0,
                            &config,
                            now + Duration::from_micros(8_333),
                        )
                        .unwrap();
                    assert!(tracker.damage_output(1, &[settled]).unwrap().0.is_none());
                }
                b.plan(&t, 55.75, 37.62, &config, now, now);
            }
            assert_eq!(updates, frames);
            assert!(*frames.iter().max().unwrap() <= 60);
            if day == 172 {
                assert!(frames[12] <= 6, "midday budget: {}", frames[12]);
            }
            if day == 319 {
                assert_eq!(&frames[..6], &[0; 6]);
            }
            eprintln!("theme {name}: samples/day={samples}; transitions/hour={updates:?}; full-output frames/hour at 60Hz or 120Hz={frames:?}");
            if let Some(dir) = std::env::var_os("GAVNO_THEME_ARTIFACTS") {
                let dir = std::path::PathBuf::from(dir);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join(format!("{name}-measurement.txt")), format!("samples/day={samples}\ntransitions/hour={updates:?}\nfull-output frames/hour at 60Hz or 120Hz={frames:?}\nBackground inline bytes={}\nView inline bytes={}\n", std::mem::size_of::<Background>(), std::mem::size_of::<View>())).unwrap();
            }
        }
        eprintln!(
            "theme two-day damage sweep elapsed: {:?}",
            started.elapsed()
        );
        if let Some(dir) = std::env::var_os("GAVNO_THEME_ARTIFACTS") {
            for (name, minute) in [
                ("night", 0),
                ("sunrise", 240),
                ("noon", 720),
                ("sunset", 1260),
            ] {
                let mut b = Background::default();
                b.intro_pending = false;
                b.update(
                    theme::palette(&time(172, minute), 55.75, 37.62),
                    Duration::ZERO,
                );
                let e = b
                    .elements(
                        &mut r,
                        &o,
                        (960, 540).into(),
                        1.0,
                        &ThemeConfig::default(),
                        Duration::ZERO,
                    )
                    .unwrap();
                // Exercise the real desktop scene builder as well as the sky shader.
                let mut h = crate::state::tests::Harness::new();
                h.gv.space.map_output(&o, (0, 0));
                h.gv.background = b;
                h.gv.background.plan(
                    &time(172, minute),
                    55.75,
                    37.62,
                    &h.gv.config.theme,
                    h.gv.start_time.elapsed(),
                    wall_time(),
                );
                h.gv.refresh_workspaces();
                h.gv.refresh_shell();
                let gv = &mut h.gv;
                let mut scene = crate::render::output_elements(
                    &mut r,
                    &gv.space,
                    &o,
                    (-1.0, -1.0).into(),
                    &mut gv.cursor_status,
                    &mut gv.cursor,
                    Duration::ZERO,
                    None,
                    &mut gv.lock,
                    &mut gv.overlay,
                    &mut gv.window_decor,
                    &mut gv.window_animations,
                    &gv.protocols.locks,
                    &mut gv.workspaces,
                    &mut gv.overview,
                    &mut gv.dock,
                    &mut gv.topbar,
                    &gv.config,
                    &mut gv.features,
                    &mut gv.media.osd,
                    &mut gv.launcher,
                    &mut gv.classic,
                    &mut gv.polkit,
                    &mut gv.background,
                );
                assert!(scene.len() > 1, "the artifact includes shell chrome");
                // Keep the separately prepared sky alive to check its shader program.
                std::hint::black_box(e);
                let bytes =
                    crate::render::capture_elements(&mut r, &scene, (960, 540), 1.0).unwrap();
                scene.clear();
                let path = std::path::PathBuf::from(&dir).join(format!("{name}.png"));
                let mut encoder = png::Encoder::new(std::fs::File::create(path).unwrap(), 960, 540);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&bytes)
                    .unwrap();
            }
        }
    }
}
