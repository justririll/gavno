//! `gavno --greeter`: the login screen, drawn by the compositor itself and run
//! by greetd as its greeter (see design/login.md).
//!
//! Layout, top to bottom: the living-theme sky, a large clock and date, a
//! card with avatar, name and password field, and power buttons at the
//! bottom. Everything moves on springs; with nothing moving GAVNO stays idle
//! (the clock wakes it once a minute).

pub mod ipc;

use std::time::Duration;

use smithay::{
    backend::renderer::{
        element::{memory::MemoryRenderBufferRenderElement, Kind},
        gles::GlesRenderer,
    },
    input::keyboard::{keysyms, Keysym, ModifiersState},
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        LoopHandle,
    },
    utils::{Logical, Physical, Point, Rectangle, Size},
};

use crate::{
    render::GavnoRenderElement,
    ui::{
        shader::{self, ShaderBox, Shaders},
        spring::{Spring, SpringParams},
        text::{TextRenderer, TextStyleSpec, Weight},
        theme::{self, LocalTime, Palette},
    },
    Gavno,
};
use ipc::{Ipc, Reply, Request, Secret};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Suspend,
    Restart,
    PowerOff,
    PrevUser,
    NextUser,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Input,
    Checking,
    /// Authenticated; fading out before asking greetd to start the session.
    LoggingIn,
    /// start_session sent.
    Starting,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MsgKind {
    Error,
    Warn,
}

#[derive(Clone)]
pub struct User {
    pub name: String,
    pub display: String,
}

const WHITE: [f32; 3] = [1.0, 1.0, 1.0];
const MAX_DOTS: usize = 14;

pub struct Greeter {
    ui_failures: u8,
    users: Vec<User>,
    selected: usize,
    password: Secret,
    phase: Phase,
    ipc: Option<Ipc>,
    lock_tx: Option<std::sync::mpsc::Sender<Secret>>,
    message: Option<(String, MsgKind)>,
    caps_lock: bool,
    layout: String,
    hostname: String,
    session_cmd: Vec<String>,
    pub quit_requested: bool,

    intro: Spring,
    shake: Spring,
    fade: Spring,
    msg: Spring,
    dots: Vec<Spring>,
    hover: Option<Hit>,
    hover_springs: [Spring; 3],
    now: Duration,

    shaders: Option<Shaders>,
    text: TextRenderer,
    bg: ShaderBox,
    card_shadow: ShaderBox,
    card: ShaderBox,
    avatar: ShaderBox,
    field: ShaderBox,
    pill: ShaderBox,
    cover: ShaderBox,
    dot_boxes: Vec<ShaderBox>,
    button_boxes: [ShaderBox; 3],
    hits: Vec<(Hit, Rectangle<f64, Logical>)>,
    lat: f64,
    lon: f64,
    palette: Option<(u32, Palette)>,
}

impl Greeter {
    pub(crate) fn safe_call<R>(&mut self, default: R, f: impl FnOnce(&mut Self) -> R) -> R {
        if self.ui_failures >= 2 { return default; }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self))) {
            Ok(value) => value,
            Err(payload) => {
                tracing::error!(message = %crate::module::panic_message(payload.as_ref()), "greeter UI panicked; retaining input gate");
                let failures = self.ui_failures + 1;
                if let Some(ipc) = &self.ipc { ipc.invalidate(); }
                self.password.clear();
                self.quit_requested = false;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    Self::create(self.ipc.take(), self.lock_tx.clone(), self.users.clone())
                }));
                if let Ok(mut ui) = result {
                    ui.ui_failures = failures;
                    *self = ui;
                } else { self.ui_failures = 2; }
                default
            }
        }
    }
    pub(crate) fn settle_intro(&mut self) { self.intro.jump(1.0); }

    pub fn new(loop_handle: &LoopHandle<'static, Gavno>) -> Self {
        Self::create(Some(Ipc::spawn(loop_handle)), None, load_users())
    }

    pub(crate) fn for_lock(tx: std::sync::mpsc::Sender<Secret>, user: User) -> Self {
        Self::create(None, Some(tx), vec![user])
    }

    fn create(ipc: Option<Ipc>, lock_tx: Option<std::sync::mpsc::Sender<Secret>>, users: Vec<User>) -> Self {
        let now = LocalTime::now();
        let (lat, lon) = theme::location(now.utc_offset);
        let mut intro = Spring::new(0.0, SpringParams { k: 90.0, zeta: 0.9 });
        intro.set(1.0);
        let session_cmd = std::env::var("GAVNO_GREETER_SESSION")
            .unwrap_or_else(|_| "gavno-session".into())
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if ipc.as_ref().is_some_and(|ipc| ipc.demo) {
            tracing::warn!("no GREETD_SOCK: demo mode, the password is \"gavno\"");
        }
        Self {
            ui_failures: 0,
            users,
            selected: 0,
            password: Secret::new(),
            phase: Phase::Input,
            ipc,
            lock_tx,
            message: None,
            caps_lock: false,
            layout: "EN".into(),
            hostname: std::fs::read_to_string("/etc/hostname")
                .map(|s| s.trim().to_owned())
                .unwrap_or_default(),
            session_cmd,
            quit_requested: false,
            intro,
            shake: Spring::new(0.0, SpringParams::SHAKE),
            fade: Spring::new(0.0, SpringParams { k: 60.0, zeta: 1.0 }),
            msg: Spring::new(0.0, SpringParams::SMOOTH),
            dots: Vec::new(),
            hover: None,
            hover_springs: std::array::from_fn(|_| Spring::new(0.0, SpringParams::SNAPPY)),
            now: Duration::ZERO,
            shaders: None,
            text: TextRenderer::new(),
            bg: ShaderBox::default(),
            card_shadow: ShaderBox::default(),
            card: ShaderBox::default(),
            avatar: ShaderBox::default(),
            field: ShaderBox::default(),
            pill: ShaderBox::default(),
            cover: ShaderBox::default(),
            dot_boxes: Vec::new(),
            button_boxes: Default::default(),
            hits: Vec::new(),
            lat,
            lon,
            palette: None,
        }
    }

    pub(crate) fn lock_checking(&self) -> bool { self.phase == Phase::Checking }

    pub(crate) fn lock_failed(&mut self) {
        self.on_reply(Reply::Denied("Wrong password".into()));
    }

    pub fn is_animating(&self) -> bool {
        self.intro.is_active()
            || self.shake.is_active()
            || self.fade.is_active()
            || self.msg.is_active()
            || self.dots.iter().any(Spring::is_active)
            || self.hover_springs.iter().any(Spring::is_active)
            || self.phase == Phase::LoggingIn
            || (self.lock_tx.is_none() && self.phase == Phase::Checking)
    }

    pub fn on_reply(&mut self, reply: Reply) {
        tracing::info!(?reply, "greetd");
        match reply {
            Reply::Authenticated => {
                self.phase = Phase::LoggingIn;
                self.message = None;
                self.fade.set(1.0);
            }
            Reply::Denied(msg) | Reply::Failed(msg) => {
                self.phase = Phase::Input;
                self.password.clear();
                self.dots.clear();
                self.shake.kick(-1500.0);
                self.message = Some((msg, MsgKind::Error));
                self.msg.set(1.0);
            }
            Reply::Started => self.quit_requested = true,
        }
    }

    pub fn set_modifiers(&mut self, caps_lock: bool, layout: &str) {
        self.caps_lock = caps_lock;
        let short: String = match layout {
            l if l.starts_with("English") => "EN".into(),
            l if l.starts_with("Russian") => "RU".into(),
            l => l.chars().take(2).collect::<String>().to_uppercase(),
        };
        self.layout = short;
        if caps_lock && self.message.as_ref().is_none_or(|(_, k)| *k == MsgKind::Warn) {
            self.message = Some(("Caps Lock is on".into(), MsgKind::Warn));
            self.msg.set(1.0);
        } else if !caps_lock && matches!(self.message, Some((_, MsgKind::Warn))) {
            self.msg.set(0.0);
        }
    }

    pub fn key(&mut self, sym: Keysym, ch: Option<char>, mods: &ModifiersState) {
        if self.phase != Phase::Input {
            return;
        }
        let raw = sym.raw();
        // Typing clears an old error.
        if matches!(self.message, Some((_, MsgKind::Error))) {
            self.msg.set(0.0);
        }
        match raw {
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => self.submit(),
            keysyms::KEY_BackSpace if mods.ctrl => self.clear_password(),
            keysyms::KEY_BackSpace => {
                self.password.pop();
                self.dots.pop();
            }
            keysyms::KEY_Escape => self.clear_password(),
            keysyms::KEY_u if mods.ctrl => self.clear_password(),
            keysyms::KEY_Left if self.users.len() > 1 => self.switch_user(-1),
            keysyms::KEY_Right if self.users.len() > 1 => self.switch_user(1),
            _ => {
                if let Some(c) = ch.filter(|c| !c.is_control()) {
                    if !mods.ctrl && !mods.logo && self.password.len() < 512 {
                        self.password.push(c);
                        let mut s = Spring::new(0.0, SpringParams::BOUNCY);
                        s.set(1.0);
                        self.dots.push(s);
                    }
                }
            }
        }
    }

    fn clear_password(&mut self) {
        self.password.clear();
        self.dots.clear();
    }

    fn switch_user(&mut self, dir: i32) {
        // While PAM checks a password the shown account must stay the one
        // being logged into.
        if self.phase != Phase::Input || self.users.is_empty() {
            return;
        }
        let n = self.users.len() as i32;
        self.selected = ((self.selected as i32 + dir).rem_euclid(n)) as usize;
        self.clear_password();
        self.intro.jump(0.6);
        self.intro.set(1.0);
    }

    fn submit(&mut self) {
        if self.password.is_empty() {
            self.shake.kick(-700.0);
            return;
        }
        let Some(user) = self.users.get(self.selected) else {
            self.message = Some(("No user accounts found".into(), MsgKind::Error));
            self.msg.set(1.0);
            return;
        };
        self.phase = Phase::Checking;
        if let Some(tx) = &self.lock_tx {
            let _ = tx.send(self.password.take());
            self.dots.clear();
        } else if let Some(ipc) = &self.ipc {
            ipc.send(Request::Login { user: user.name.clone(), password: self.password.take() });
        }
    }

    pub fn pointer_motion(&mut self, pos: Point<f64, Logical>) -> bool {
        let hover = self.hit(pos);
        let changed = hover != self.hover;
        self.hover = hover;
        changed
    }

    pub fn click(&mut self, pos: Point<f64, Logical>) {
        let Some(hit) = self.hit(pos) else { return };
        match hit {
            Hit::PrevUser => self.switch_user(-1),
            Hit::NextUser => self.switch_user(1),
            Hit::Suspend => crate::state::spawn_shell("systemctl suspend"),
            Hit::Restart => crate::state::spawn_shell("systemctl reboot"),
            Hit::PowerOff => crate::state::spawn_shell("systemctl poweroff"),
        }
    }

    pub(crate) fn hit(&self, pos: Point<f64, Logical>) -> Option<Hit> {
        self.hits.iter().find(|(_, r)| r.contains(pos)).map(|(h, _)| *h)
    }

    /// After the fade-out completes, ask greetd to start the session.
    fn advance_phase(&mut self) {
        if self.phase == Phase::LoggingIn && self.fade.value > 0.97 {
            self.phase = Phase::Starting;
            if let Some(ipc) = &self.ipc {
                ipc.send(Request::Start { cmd: self.session_cmd.clone(), env: vec![] });
            } else {
                self.quit_requested = true;
            }
        }
    }

    fn current_palette(&mut self, t: &LocalTime) -> Palette {
        let key = t.yday * 1440 + t.hour * 60 + t.minute;
        match self.palette {
            Some((k, p)) if k == key => p,
            _ => {
                let p = theme::palette(t, self.lat, self.lon);
                self.palette = Some((key, p));
                p
            }
        }
    }

    /// Everything for one output, front to back.
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
    ) -> Vec<GavnoRenderElement> {
        self.now = now;
        if self.shaders.is_none() {
            match Shaders::compile(renderer) {
                Ok(s) => self.shaders = Some(s),
                Err(err) => {
                    tracing::error!(?err, "greeter shaders failed to compile");
                    return Vec::new();
                }
            }
        }
        let intro = self.intro.advance(now);
        let shake = self.shake.advance(now);
        let fade = self.fade.advance(now);
        let msg_alpha = self.msg.advance(now) as f32;
        for d in &mut self.dots {
            d.advance(now);
        }
        for (i, s) in self.hover_springs.iter_mut().enumerate() {
            let target = match (i, self.hover) {
                (0, Some(Hit::Suspend)) | (1, Some(Hit::Restart)) | (2, Some(Hit::PowerOff)) => 1.0,
                _ => 0.0,
            };
            s.set(target);
            s.advance(now);
        }
        if !self.msg.is_active() && self.msg.target == 0.0 {
            self.message = None;
        }
        self.advance_phase();

        let t = LocalTime::now();
        let pal = self.current_palette(&t);
        let shaders = self.shaders.take().unwrap();
        let mut back: Vec<GavnoRenderElement> = Vec::new();
        let mut hits = Vec::new();

        let (w, h) = (size.w as f64, size.h as f64);
        let cx = w / 2.0;
        let ui_alpha = (intro * (1.0 - fade)).clamp(0.0, 1.0) as f32;
        let full = Rectangle::from_size(size);

        // Sky.
        back.push(GavnoRenderElement::Shader(self.bg.element(
            &shaders.background,
            full,
            if self.lock_tx.is_some() { (intro * (1.0 - fade)).clamp(0.0, 1.0) as f32 } else { 1.0 },
            vec![
                ("scale", vec![scale as f32]),
                ("sky_top", pal.sky_top.to_vec()),
                ("sky_bottom", pal.sky_bottom.to_vec()),
                ("blob_a", pal.blob_a.to_vec()),
                ("blob_b", pal.blob_b.to_vec()),
                ("glow_color", pal.glow.to_vec()),
                ("sun", pal.sun.to_vec()),
            ],
        )));

        // Clock and date.
        let clock_y = h * 0.12 - (1.0 - intro) * 12.0;
        let clock = format!("{:02}:{:02}", t.hour, t.minute);
        let clock_h = self.push_text(
            renderer,
            &mut back,
            &clock,
            style(112.0, Weight::Thin, WHITE, 0.96, -2.0),
            Align::Center(cx),
            clock_y,
            ui_alpha,
            scale,
        );
        let date = format!("{}, {} {}", t.weekday_name(), t.month_name(), t.day);
        self.push_text(
            renderer,
            &mut back,
            &date,
            style(21.0, Weight::Light, WHITE, 0.78, 0.4),
            Align::Center(cx),
            clock_y + clock_h - 6.0,
            ui_alpha,
            scale,
        );

        // Card.
        let card_w = 380.0;
        let card_h = 284.0;
        let card_x = cx - card_w / 2.0 + shake;
        let card_y = h * 0.43 + (1.0 - intro) * 36.0 - fade * 14.0;
        let card = rect(card_x, card_y, card_w, card_h);
        back.push(GavnoRenderElement::Shader(shader::shadow(
            &mut self.card_shadow,
            &shaders,
            card,
            30.0,
            44,
            20,
            [0.0, 0.0, 0.04, 0.32],
            ui_alpha,
            scale,
        )));
        back.push(GavnoRenderElement::Shader(shader::rrect(
            &mut self.card,
            &shaders,
            card,
            30.0 * scale as f32,
            [1.0, 1.0, 1.0, 0.11],
            1.2 * scale as f32,
            [1.0, 1.0, 1.0, 0.20],
            ui_alpha,
            scale,
        )));

        // Avatar with the initial.
        let (name, display) = self
            .users
            .get(self.selected)
            .map(|u| (u.name.clone(), u.display.clone()))
            .unwrap_or_else(|| ("".into(), "No users".into()));
        let av = 92.0;
        let av_y = card_y + 30.0;
        let avatar = rect(card_x + card_w / 2.0 - av / 2.0, av_y, av, av);
        let accent = pal.accent;
        back.push(GavnoRenderElement::Shader(shader::rrect(
            &mut self.avatar,
            &shaders,
            avatar,
            (av / 2.0 * scale) as f32,
            [accent[0], accent[1], accent[2], 0.85],
            2.0 * scale as f32,
            [1.0, 1.0, 1.0, 0.45],
            ui_alpha,
            scale,
        )));
        let initial: String = display.chars().next().or(name.chars().next()).unwrap_or('?').to_uppercase().collect();
        let ih = self.text_height(&initial, style(40.0, Weight::Medium, WHITE, 1.0, 0.0), scale);
        self.push_text(
            renderer,
            &mut back,
            &initial,
            style(40.0, Weight::Medium, WHITE, 1.0, 0.0),
            Align::Center(card_x + card_w / 2.0),
            av_y + (av - ih) / 2.0,
            ui_alpha,
            scale,
        );

        // Name, with arrows when there are several accounts.
        let name_y = av_y + av + 14.0;
        let name_h = self.push_text(
            renderer,
            &mut back,
            &display,
            style(22.0, Weight::Medium, WHITE, 0.96, 0.0),
            Align::Center(card_x + card_w / 2.0),
            name_y,
            ui_alpha,
            scale,
        );
        if self.users.len() > 1 {
            let arrow = style(26.0, Weight::Light, WHITE, 0.7, 0.0);
            for (hit, glyph, dx) in [(Hit::PrevUser, "‹", -140.0), (Hit::NextUser, "›", 140.0)] {
                let x = card_x + card_w / 2.0 + dx;
                self.push_text(renderer, &mut back, glyph, arrow, Align::Center(x), name_y - 3.0, ui_alpha, scale);
                hits.push((hit, Rectangle::new((x - 20.0, name_y - 6.0).into(), (40.0, 40.0).into())));
            }
        }

        // Password field.
        let fw = 300.0;
        let fh = 48.0;
        let fx = card_x + (card_w - fw) / 2.0;
        let fy = name_y + name_h + 16.0;
        let focused = self.phase == Phase::Input;
        let border = if focused {
            [accent[0], accent[1], accent[2], 0.95]
        } else {
            [1.0, 1.0, 1.0, 0.25]
        };
        back.push(GavnoRenderElement::Shader(shader::rrect(
            &mut self.field,
            &shaders,
            rect(fx, fy, fw, fh),
            (fh / 2.0 * scale) as f32,
            [0.0, 0.0, 0.0, 0.22],
            1.6 * scale as f32,
            border,
            ui_alpha,
            scale,
        )));

        if self.dots.is_empty() && self.phase == Phase::Input {
            let ph = style(16.0, Weight::Light, WHITE, 0.5, 0.2);
            let th = self.text_height("Password", ph, scale);
            self.push_text(
                renderer,
                &mut back,
                "Password",
                ph,
                Align::Left(fx + 22.0),
                fy + (fh - th) / 2.0,
                ui_alpha,
                scale,
            );
        }

        // Dots: pop in on a bouncy spring; a wave runs through them while
        // PAM checks the password.
        let shown = self.dots.len().min(MAX_DOTS);
        let first = self.dots.len() - shown;
        if self.dot_boxes.len() < shown {
            self.dot_boxes.resize_with(shown, ShaderBox::default);
        }
        let checking = matches!(self.phase, Phase::Checking | Phase::LoggingIn | Phase::Starting);
        for i in 0..shown {
            let s = self.dots[first + i].value.max(0.0);
            let d = 11.0 * s;
            let ccx = fx + 24.0 + i as f64 * 18.0;
            let ccy = fy + fh / 2.0;
            let wave = if checking {
                let ph = now.as_secs_f64() * 7.0 - i as f64 * 0.55;
                0.35 + 0.65 * (0.5 + 0.5 * ph.sin())
            } else {
                1.0
            };
            if d < 0.5 {
                continue;
            }
            back.push(GavnoRenderElement::Shader(shader::rrect(
                &mut self.dot_boxes[i],
                &shaders,
                rect(ccx - d / 2.0, ccy - d / 2.0, d, d),
                (d / 2.0 * scale) as f32,
                [1.0, 1.0, 1.0, 0.95 * wave as f32],
                0.0,
                [0.0; 4],
                ui_alpha,
                scale,
            )));
        }

        // Keyboard layout pill inside the field.
        let pill_w = 36.0;
        let pill_h = 24.0;
        let pill_x = fx + fw - pill_w - 12.0;
        let pill_y = fy + (fh - pill_h) / 2.0;
        back.push(GavnoRenderElement::Shader(shader::rrect(
            &mut self.pill,
            &shaders,
            rect(pill_x, pill_y, pill_w, pill_h),
            (pill_h / 2.0 * scale) as f32,
            [1.0, 1.0, 1.0, 0.13],
            0.0,
            [0.0; 4],
            ui_alpha,
            scale,
        )));
        let ls = style(12.0, Weight::Medium, WHITE, 0.8, 0.8);
        let layout = self.layout.clone();
        let lh = self.text_height(&layout, ls, scale);
        self.push_text(
            renderer,
            &mut back,
            &layout,
            ls,
            Align::Center(pill_x + pill_w / 2.0),
            pill_y + (pill_h - lh) / 2.0,
            ui_alpha,
            scale,
        );

        // Message line.
        if let Some((text, kind)) = self.message.clone() {
            let color = match kind {
                MsgKind::Error => [1.0, 0.72, 0.66],
                MsgKind::Warn => [1.0, 0.88, 0.55],
            };
            self.push_text(
                renderer,
                &mut back,
                &text,
                style(14.0, Weight::Regular, color, 0.95, 0.2),
                Align::Center(card_x + card_w / 2.0),
                fy + fh + 12.0 - (1.0 - msg_alpha as f64) * 6.0,
                ui_alpha * msg_alpha,
                scale,
            );
        }

        // Bottom: host on the left, power buttons on the right.
        let base = h - 30.0;
        let host = if self.hostname.is_empty() { "GAVNO".to_owned() } else { format!("GAVNO  ·  {}", self.hostname) };
        let hs = style(14.0, Weight::Medium, WHITE, 0.55, 0.6);
        let hh = self.text_height(&host, hs, scale);
        self.push_text(renderer, &mut back, &host, hs, Align::Left(30.0), base - hh, ui_alpha, scale);

        let labels = [(Hit::Suspend, "Suspend"), (Hit::Restart, "Restart"), (Hit::PowerOff, "Shut down")];
        let bs = style(14.0, Weight::Regular, WHITE, 0.88, 0.2);
        let bh = 36.0;
        let mut x = w - 30.0;
        for (i, (hit, label)) in labels.iter().enumerate().rev() {
            let tw = self.text.measure(label, bs) as f64;
            let bw = tw + 34.0;
            x -= bw;
            let r = rect(x, base - bh, bw, bh);
            let hov = self.hover_springs[i].value as f32;
            back.push(GavnoRenderElement::Shader(shader::rrect(
                &mut self.button_boxes[i],
                &shaders,
                r,
                (bh / 2.0 * scale) as f32,
                [1.0, 1.0, 1.0, 0.08 + 0.14 * hov],
                1.0 * scale as f32,
                [1.0, 1.0, 1.0, 0.10 + 0.15 * hov],
                ui_alpha,
                scale,
            )));
            let th = self.text_height(label, bs, scale);
            self.push_text(renderer, &mut back, label, bs, Align::Center(x + bw / 2.0), base - bh + (bh - th) / 2.0, ui_alpha, scale);
            hits.push((*hit, Rectangle::new((x, base - bh).into(), (bw, bh).into())));
            x -= 10.0;
        }

        // Fade to the session's background color on success.
        if fade > 0.001 && self.lock_tx.is_none() {
            let c = crate::render::CLEAR_COLOR;
            back.push(GavnoRenderElement::Shader(shader::rrect(
                &mut self.cover,
                &shaders,
                full,
                0.0,
                c,
                0.0,
                [0.0; 4],
                fade.clamp(0.0, 1.0) as f32,
                scale,
            )));
        }

        self.hits = hits;
        self.shaders = Some(shaders);
        back.reverse();
        back
    }

    fn text_height(&mut self, s: &str, st: TextStyleSpec, scale: f64) -> f64 {
        self.text.render(s, st, scale).map(|i| i.logical_h as f64).unwrap_or(0.0)
    }

    /// Adds a text element; returns its logical height.
    #[allow(clippy::too_many_arguments)]
    fn push_text(
        &mut self,
        renderer: &mut GlesRenderer,
        out: &mut Vec<GavnoRenderElement>,
        s: &str,
        st: TextStyleSpec,
        align: Align,
        y: f64,
        alpha: f32,
        scale: f64,
    ) -> f64 {
        let Some(img) = self.text.render(s, st, scale) else { return 0.0 };
        let lw = img.logical_w as f64;
        let x = match align {
            Align::Left(x) => x,
            Align::Center(cx) => cx - lw / 2.0,
        };
        let loc = Point::<f64, Physical>::from(((x * scale).round(), (y * scale).round()));
        if alpha > 0.002 {
            if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                loc,
                &img.buffer,
                Some(alpha),
                // The whole raster, scaled to the logical size (without an
                // explicit src, a size would crop instead of scale).
                Some(Rectangle::from_size((img.width as f64, img.height as f64).into())),
                Some((img.logical_w, img.logical_h).into()),
                Kind::Unspecified,
            ) {
                out.push(GavnoRenderElement::Memory(elem));
            }
        }
        img.logical_h as f64
    }
}

#[derive(Clone, Copy)]
enum Align {
    Left(f64),
    Center(f64),
}

fn style(size: f32, weight: Weight, rgb: [f32; 3], a: f32, tracking: f32) -> TextStyleSpec {
    TextStyleSpec {
        size,
        weight,
        color: [rgb[0], rgb[1], rgb[2], a],
        tracking,
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (x.round() as i32, y.round() as i32).into(),
        (w.round() as i32, h.round() as i32).into(),
    )
}

/// Human accounts from /etc/passwd: uid 1000..60000 with a real shell.
fn load_users() -> Vec<User> {
    let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else {
        return Vec::new();
    };
    passwd
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            let uid: u32 = f[2].parse().ok()?;
            let shell = f[6];
            if !(1000..60000).contains(&uid) || shell.ends_with("nologin") || shell.ends_with("false") {
                return None;
            }
            let gecos = f[4].split(',').next().unwrap_or("").trim();
            let display = if gecos.is_empty() { f[0].to_owned() } else { gecos.to_owned() };
            Some(User {
                name: f[0].to_owned(),
                display,
            })
        })
        .collect()
}

/// Starts greeter mode: creates the greeter and a timer that redraws at
/// every minute boundary for the clock.
pub fn start(gv: &mut Gavno) {
    gv.greeter = Some(Greeter::new(&gv.loop_handle));
    let until_next_minute = |t: LocalTime| Duration::from_secs(60 - t.second as u64 % 60) + Duration::from_millis(20);
    let first = until_next_minute(LocalTime::now());
    let _ = gv.loop_handle.insert_source(Timer::from_duration(first), move |_, _, gv| {
        let _stall = crate::perf::Stall::new("timer.greeter_clock");
        if gv.greeter.as_ref().is_none_or(|g| g.ui_failures >= 2) { return TimeoutAction::Drop; }
        gv.queue_redraw_all();
        TimeoutAction::ToDuration(until_next_minute(LocalTime::now()))
    });
}

#[cfg(test)]
mod lock_ui_tests {
    use super::*;
    #[test]
    fn lock_ui_has_one_user_and_no_greetd_or_demo_backend() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut ui = Greeter::for_lock(tx, User { name: "test".into(), display: "Test User".into() });
        assert!(ui.ipc.is_none()); assert_eq!(ui.users.len(), 1);
        let mods = ModifiersState::default();
        ui.key(Keysym::from(keysyms::KEY_Left), None, &mods);
        assert_eq!(ui.selected, 0);
        ui.key(Keysym::from(keysyms::KEY_a), Some('a'), &mods);
        ui.key(Keysym::from(keysyms::KEY_Return), None, &mods);
        assert_eq!(rx.try_recv().unwrap().as_str(), "a");
        assert!(ui.password.is_empty()); assert!(ui.dots.is_empty());
        assert_eq!(ui.phase, Phase::Checking);
        ui.intro.jump(1.0);
        assert!(!ui.is_animating(), "a blocked PAM worker must not request frames");
        ui.key(Keysym::from(keysyms::KEY_a), Some('a'), &mods);
        assert!(ui.password.is_empty());
    }
    #[test]
    fn lock_error_springs_settle_and_success_completes_without_starting_a_session() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut ui = Greeter::for_lock(tx, User { name: "test".into(), display: "Test User".into() });
        ui.intro.jump(1.0);
        ui.lock_failed();
        assert!(ui.is_animating());
        assert_eq!(ui.message, Some(("Wrong password".into(), MsgKind::Error)));
        for tick in 0..500 {
            let now = Duration::from_millis(tick * 20);
            ui.shake.advance(now); ui.msg.advance(now);
        }
        assert!(!ui.is_animating());
        ui.on_reply(Reply::Authenticated);
        for tick in 0..500 {
            ui.fade.advance(Duration::from_millis(10000 + tick * 20));
            ui.advance_phase();
        }
        assert!(ui.quit_requested); assert_eq!(ui.phase, Phase::Starting);
        assert!(!ui.is_animating());
    }
}
