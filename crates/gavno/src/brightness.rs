//! Global software brightness, on-demand animation and off-thread persistence.
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Sender},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use smithay::{
    backend::renderer::{
        element::{solid::SolidColorRenderElement, Id, Kind},
        utils::CommitCounter,
    },
    output::Output,
    utils::Rectangle,
};

use crate::{
    topbar::status::Brightness,
    ui::spring::{Spring, SpringParams},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Control {
    #[default]
    Auto,
    Backlight,
    Software,
}
impl Control {
    pub fn software(self, backlight: bool) -> bool {
        self == Self::Software || (self == Self::Auto && !backlight)
    }
}

pub struct State {
    pub backlight: Option<Brightness>,
    pub software: bool,
    level: f64,
    spring: Spring,
    id: Id,
    commit: CommitCounter,
    alpha: f32,
    path: Option<PathBuf>,
    writer: Option<Sender<f64>>,
}
impl Default for State {
    fn default() -> Self {
        Self::new(1.0, None)
    }
}
impl State {
    fn new(level: f64, path: Option<PathBuf>) -> Self {
        Self {
            backlight: None,
            software: false,
            level: clamp(level),
            spring: Spring::new(
                1.0,
                SpringParams {
                    k: 10000.0,
                    zeta: 1.0,
                },
            ),
            id: Id::new(),
            commit: CommitCounter::default(),
            alpha: 0.0,
            path,
            writer: None,
        }
    }
    /// Startup only; subsequent disk writes never run on the compositor thread.
    pub fn load(control: Control) -> Self {
        let path = state_path();
        let level = path.as_deref().and_then(restore).unwrap_or(1.0);
        let mut state = Self::new(level, if cfg!(test) { None } else { path });
        state.backlight = crate::topbar::status::read_brightness(Path::new("/sys/class/backlight"));
        state.configure(control, Duration::ZERO);
        // The first frame already uses the restored level, without a bright flash.
        state
            .spring
            .jump(if state.software { state.level } else { 1.0 });
        state
    }
    pub fn configure(&mut self, control: Control, now: Duration) {
        let software = control.software(self.backlight.is_some());
        if self.software == software {
            return;
        }
        self.software = software;
        if software {
            self.spring.advance(now);
            self.spring.set(self.level);
        } else {
            // A hardware-controlled screen must not retain any software dimming.
            self.spring.jump(1.0);
        }
    }
    pub fn snapshot(&self) -> Option<Brightness> {
        if self.software {
            Some(Brightness {
                name: "software".into(),
                value: (self.level * 10000.0).round() as u32,
                max: 10000,
            })
        } else {
            self.backlight.clone()
        }
    }
    pub fn level(&self) -> f64 {
        self.level
    }
    pub fn set(&mut self, level: f64, now: Duration) {
        let level = clamp(level);
        if level == self.level {
            return;
        }
        self.spring.advance(now);
        self.level = level;
        self.spring.set(level);
        if self.writer.is_none() {
            if let Some(path) = &self.path {
                self.writer = spawn_writer(path.clone());
            }
        }
        if let Some(writer) = &self.writer {
            let _ = writer.send(level);
        }
    }
    pub fn active(&self) -> bool {
        self.spring.is_active()
    }
    pub fn dimmed(&self) -> bool {
        self.spring.value < 1.0 || self.spring.target < 1.0
    }
    pub fn element(&mut self, output: &Output, now: Duration) -> Option<SolidColorRenderElement> {
        let factor = clamp(self.spring.advance(now));
        let alpha = (1.0 - factor) as f32;
        if self.alpha != alpha {
            self.alpha = alpha;
            self.commit.increment();
        }
        if alpha == 0.0 {
            return None;
        }
        let mode = output.current_mode()?;
        let size = output.current_transform().transform_size(mode.size);
        Some(SolidColorRenderElement::new(
            self.id.clone(),
            Rectangle::from_size(size),
            self.commit,
            [0.0, 0.0, 0.0, alpha],
            Kind::Unspecified,
        ))
    }
}
fn clamp(level: f64) -> f64 {
    if level.is_finite() {
        level.clamp(0.05, 1.0)
    } else {
        1.0
    }
}
fn state_path() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .map(|p| p.join("gavno/brightness"))
}
fn restore(path: &Path) -> Option<f64> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(64)
        .read_to_string(&mut text)
        .ok()?;
    let level = text.trim().parse::<f64>().ok()?;
    level.is_finite().then(|| clamp(level))
}
fn persist(path: &Path, level: f64) -> std::io::Result<()> {
    use std::io::Write;
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        writeln!(file, "{level:.6}")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
fn spawn_writer(path: PathBuf) -> Option<Sender<f64>> {
    let (sender, receiver) = mpsc::channel();
    match std::thread::Builder::new()
        .name("brightness-state".into())
        .spawn(move || {
            while let Ok(mut level) = receiver.recv() {
                // Coalesce slider motion; sleep in recv when idle, without a timer.
                loop {
                    match receiver.recv_timeout(Duration::from_millis(200)) {
                        Ok(next) => level = next,
                        Err(_) => break,
                    }
                }
                if let Err(error) = persist(&path, level) {
                    tracing::warn!(%error, "cannot save software brightness");
                }
            }
        }) {
        Ok(_) => Some(sender),
        Err(error) => {
            tracing::warn!(%error, "cannot start brightness persistence worker");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::{
        backend::renderer::element::Element,
        output::{Mode, PhysicalProperties, Subpixel},
    };

    #[test]
    fn auto_resolution_and_mode_switch_preserve_software_level() {
        assert!(Control::Auto.software(false));
        assert!(!Control::Auto.software(true));
        assert!(Control::Software.software(true));
        assert!(!Control::Backlight.software(false));
        let mut state = State::default();
        state.configure(Control::Software, Duration::ZERO);
        state.set(0.3, Duration::ZERO);
        assert_eq!(state.snapshot().unwrap().level(), 0.3);
        state.configure(Control::Backlight, Duration::from_secs(1));
        assert_eq!(state.spring.value, 1.0);
        assert!(!state.active());
        assert!(state.snapshot().is_none());
        state.configure(Control::Software, Duration::from_secs(2));
        assert_eq!(state.level(), 0.3);
        assert!(state.active());
    }
    #[test]
    fn persistence_round_trip_and_invalid_state() {
        let dir = std::env::temp_dir().join(format!("gavno-brightness-{}", std::process::id()));
        let path = dir.join("brightness");
        persist(&path, 0.275).unwrap();
        assert_eq!(restore(&path), Some(0.275));
        std::fs::write(&path, "0").unwrap();
        assert_eq!(restore(&path), Some(0.05));
        std::fs::write(&path, "NaN").unwrap();
        assert_eq!(restore(&path), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn overlay_blends_and_damage_tracker_returns_to_idle() {
        use smithay::backend::{
            allocator::Fourcc,
            egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
            renderer::{
                damage::OutputDamageTracker,
                gles::{GlesRenderer, GlesTexture},
                Bind, ExportMem, Offscreen,
            },
        };
        use smithay::utils::Transform;
        let display =
            unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.expect("surfaceless EGL display");
        let context = EGLContext::new(&display).expect("surfaceless EGL context");
        let mut renderer = unsafe { GlesRenderer::new(context) }.expect("GLES renderer");
        let output = Output::new(
            "brightness-pixels".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: (16, 8).into(),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );
        let mut state = State::default();
        state.configure(Control::Software, Duration::ZERO);
        state.set(0.4, Duration::ZERO);
        state.spring.jump(0.4);
        let mut elements = Vec::new();
        crate::render::dim_output(&mut elements, &mut state, &output, Duration::ZERO);
        assert!(matches!(
            elements.first(),
            Some(crate::render::GavnoRenderElement::Brightness(_))
        ));
        let mut texture: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, (16, 8).into())
            .unwrap();
        let mut tracker = OutputDamageTracker::new((16, 8), 1.0, Transform::Normal);
        let mapping = {
            let mut fb = renderer.bind(&mut texture).unwrap();
            assert!(tracker
                .render_output(&mut renderer, &mut fb, 0, &elements, [1.0; 4])
                .unwrap()
                .damage
                .is_some());
            assert!(tracker
                .render_output(&mut renderer, &mut fb, 1, &elements, [1.0; 4])
                .unwrap()
                .damage
                .is_none());
            renderer
                .copy_framebuffer(&fb, Rectangle::from_size((16, 8).into()), Fourcc::Abgr8888)
                .unwrap()
        };
        let pixels = renderer.map_texture(&mapping).unwrap();
        assert!(pixels
            .chunks_exact(4)
            .all(|p| p[..3].iter().all(|v| (i32::from(*v) - 102).abs() <= 1)));
        // Removing the display-only pass restores the undimmed content.
        let mut fb = renderer.bind(&mut texture).unwrap();
        elements.clear();
        assert!(tracker
            .render_output(&mut renderer, &mut fb, 1, &elements, [1.0; 4])
            .unwrap()
            .damage
            .is_some());
    }
    #[test]
    fn overlay_has_stable_identity_and_no_idle_damage() {
        let output = Output::new(
            "brightness-test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: (1920, 1080).into(),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );
        let mut state = State::default();
        assert!(state.element(&output, Duration::ZERO).is_none());
        state.configure(Control::Software, Duration::ZERO);
        state.set(0.4, Duration::ZERO);
        let mut previous = 1.0;
        for i in 1..=24 {
            let element = state
                .element(&output, Duration::from_millis(i * 8))
                .unwrap();
            let factor = 1.0 - element.alpha();
            assert!(factor <= previous && factor >= 0.4 - 0.00001);
            previous = factor;
        }
        assert!(!state.active(), "brightness settles in about 150 ms");
        let a = state.element(&output, Duration::from_secs(1)).unwrap();
        let b = state.element(&output, Duration::from_secs(2)).unwrap();
        assert_eq!(a.id(), b.id());
        assert_eq!(a.current_commit(), b.current_commit());
        state.set(0.2, Duration::from_secs(2));
        let c = state.element(&output, Duration::from_millis(2050)).unwrap();
        assert_eq!(a.id(), c.id());
        assert_ne!(a.current_commit(), c.current_commit());
        state.configure(Control::Backlight, Duration::from_secs(3));
        assert!(state.element(&output, Duration::from_secs(3)).is_none());
    }
}
