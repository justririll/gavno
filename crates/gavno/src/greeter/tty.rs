//! The lock screen as a console login: `host login: user`, a silent
//! `Password:` prompt and `Login incorrect`. Only the look differs; every key
//! and the authentication still go through the regular greeter state.

use super::*;
use crate::topbar::{geometry::rect, Canvas};

const FONT: f32 = 15.0;
const LINE_H: i32 = 20;
const MARGIN: i32 = 14;
const GREY: [f32; 4] = [0.78, 0.78, 0.81, 1.0];
const BLACK: [f32; 4] = [0.039, 0.039, 0.047, 1.0];

fn mono(color: [f32; 4]) -> TextStyleSpec {
    TextStyleSpec { size: FONT, weight: Weight::Mono, color, tracking: 0.0 }
}

/// What the console says for a failure message of the regular lock screen.
pub(crate) fn failure_text(message: &str) -> &str {
    if message == "Wrong password" { "Login incorrect" } else { message }
}

impl Greeter {
    /// The login lines, top to bottom.
    pub(crate) fn tty_lines(&self) -> Vec<String> {
        let host = if self.hostname.is_empty() { "localhost" } else { &self.hostname };
        let user = self.users.get(self.selected).map(|u| u.name.as_str()).unwrap_or("user");
        let mut lines = vec![format!("{host} login: {user}"), "Password: ".to_owned()];
        if let Some((message, MsgKind::Error | MsgKind::Warn)) = &self.message {
            lines.push(failure_text(message).to_owned());
        }
        lines
    }

    pub(super) fn elements_tty(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
    ) -> Vec<GavnoRenderElement> {
        self.now = now;
        let intro = self.intro.advance(now);
        self.shake.advance(now);
        let fade = self.fade.advance(now);
        let msg_alpha = self.msg.advance(now) as f32;
        for d in &mut self.dots {
            d.advance(now);
        }
        if !self.msg.is_active() && self.msg.target == 0.0 {
            self.message = None;
        }
        self.advance_phase();
        self.hits.clear();
        let alpha = (intro * (1.0 - fade)).clamp(0.0, 1.0) as f32;
        let lines = self.tty_lines();
        let checking = self.phase != Phase::Input;
        let c = self.tty_canvas.get_or_insert_with(Canvas::default);
        c.begin(renderer, scale);
        c.alpha = alpha;
        c.rect(rect(0, 0, size.w, size.h), 0.0, BLACK, 0.0, [0.0; 4]);
        for (i, line) in lines.iter().enumerate() {
            let failure = i >= 2;
            c.alpha = if failure { alpha * msg_alpha.max(0.05) } else { alpha };
            let width = c.text.measure(line, mono(GREY)).ceil() as i32 + 4;
            c.styled_label(renderer, line, MARGIN, MARGIN + i as i32 * LINE_H, width, mono(GREY));
        }
        c.alpha = alpha;
        if !checking {
            let x = MARGIN + c.text.measure(&lines[1], mono(GREY)).ceil() as i32;
            let cell = c.text.measure("M", mono(GREY)).ceil() as i32;
            c.rect(rect(x, MARGIN + LINE_H + 2, cell, LINE_H - 3), 0.0, [GREY[0], GREY[1], GREY[2], 0.9], 0.0, [0.0; 4]);
        }
        c.finish()
    }
}
