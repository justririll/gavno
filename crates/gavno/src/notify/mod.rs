//! Notification state belongs to the event loop; D-Bus never borrows compositor state.
pub mod dbus;
mod ui;

use crate::{
    module::Feature,
    topbar::{geometry::Rect, Canvas},
    ui::spring::{Spring, SpringParams},
    Gavno,
};
use smithay::{
    output::Output,
    reexports::calloop::{
        channel,
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    utils::{Logical, Point},
};
use std::{cell::RefCell, collections::VecDeque, rc::Rc, sync::mpsc, time::Duration};

pub type Shared = Rc<RefCell<Notifications>>;

#[derive(Clone, Debug)]
pub struct Notice {
    pub id: u32,
    pub app: String,
    pub icon: String,
    pub desktop: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>,
    pub urgency: u8,
    pub timeout: i32,
}
#[derive(Clone, Debug)]
pub struct History {
    pub notice: Notice,
    pub time: String,
}
#[derive(Debug)]
pub enum Request {
    Notify(Notice),
    Close(u32),
    Active,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Signal {
    Closed(u32, u32),
    Action(u32, String),
    HistoryAction(u32),
}

struct Card {
    notice: Notice,
    expires: Option<Duration>,
    hovered: Option<Duration>,
    visible: bool,
    closing: bool,
    spring: Spring,
    rect: Option<Rect>,
    buttons: Vec<(Rect, String)>,
}
#[derive(Default)]
pub struct Notifications {
    cards: Vec<Card>,
    pub history: VecDeque<History>,
    pub unread: bool,
    pub dnd: bool,
    active: bool,
    started: bool,
    blocked: bool,
    output: Option<Output>,
    timer: Option<(Duration, RegistrationToken)>,
    pub signals: Option<mpsc::Sender<Signal>>,
    canvas: Canvas,
    icons: crate::icons::Icons,
    entries: crate::desktop_entry::DesktopEntries,
}

/// Strip only the supported simple markup, then decode entities once. Unknown
/// tags and malformed angle brackets remain readable instead of eating text.
pub fn plain_body(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if let Some(end) = rest.find('>') {
            let tag = rest[1..end].trim();
            if matches!(tag, "b" | "/b" | "i" | "/i" | "u" | "/u" | "a" | "/a")
                || tag.starts_with("a ")
            {
                rest = &rest[end + 1..];
                continue;
            }
        }
        out.push('<');
        rest = &rest[1..];
    }
    out.push_str(rest);
    let mut decoded = String::new();
    let mut rest = out.as_str();
    while let Some(i) = rest.find('&') {
        decoded.push_str(&rest[..i]);
        rest = &rest[i..];
        if let Some((entity, value)) = [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&apos;", '\''),
        ]
        .into_iter()
        .find(|(e, _)| rest.starts_with(e))
        {
            decoded.push(value);
            rest = &rest[entity.len()..];
        } else {
            decoded.push('&');
            rest = &rest[1..];
        }
    }
    decoded.push_str(rest);
    decoded
}

impl Notifications {
    fn emit(&self, signal: Signal) {
        if let Some(tx) = &self.signals {
            let _ = tx.send(signal);
        }
    }
    fn receive(&mut self, mut n: Notice, now: Duration, default_ms: u64) {
        n.body = plain_body(&n.body);
        let ttl = if n.urgency == 2 || n.timeout == 0 {
            None
        } else {
            Some(Duration::from_millis(if n.timeout < 0 {
                default_ms
            } else {
                n.timeout as u64
            }))
        };
        let expires = ttl.map(|t| now.saturating_add(t));
        let clock = crate::ui::theme::LocalTime::now();
        let history = History {
            notice: n.clone(),
            time: format!("{:02}:{:02}", clock.hour, clock.minute),
        };
        self.history.retain(|h| h.notice.id != n.id);
        self.history.push_front(history);
        self.history.truncate(50);
        self.unread = true;
        if let Some(card) = self
            .cards
            .iter_mut()
            .find(|c| c.notice.id == n.id && !c.closing)
        {
            card.notice = n;
            card.expires = expires;
            card.hovered = card.hovered.map(|_| now);
        } else {
            self.cards.push(Card {
                notice: n,
                expires,
                hovered: None,
                visible: false,
                closing: false,
                spring: Spring::new(0.0, SpringParams::SNAPPY),
                rect: None,
                buttons: Vec::new(),
            });
        }
    }
    fn close(&mut self, id: u32, reason: Option<u32>) {
        if let Some(card) = self
            .cards
            .iter_mut()
            .find(|c| c.notice.id == id && !c.closing)
        {
            card.closing = true;
            card.spring.set(0.0);
            card.rect = None;
            if let Some(reason) = reason {
                self.emit(Signal::Closed(id, reason));
            }
        }
    }
    fn update(&mut self, now: Duration, blocked: bool, max_visible: usize) -> bool {
        let before: Vec<_> = self
            .cards
            .iter()
            .map(|c| (c.notice.id, c.visible, c.closing))
            .collect();
        // A hidden card cannot remain hovered. Preserve time already paused,
        // then let queued cards age without installing hidden timers.
        for card in &mut self.cards {
            if blocked || (self.dnd && card.notice.urgency != 2) {
                if let Some(start) = card.hovered.take() {
                    card.expires = card
                        .expires
                        .map(|t| t.saturating_add(now.saturating_sub(start)));
                }
            }
        }
        let expired: Vec<_> = self
            .cards
            .iter()
            .filter(|c| !c.closing && c.hovered.is_none() && c.expires.is_some_and(|t| t <= now))
            .map(|c| c.notice.id)
            .collect();
        for id in expired {
            self.close(id, Some(1));
        }
        self.cards
            .retain(|c| !(c.closing && (!c.visible || !c.spring.is_active())));
        self.blocked = blocked;
        let mut slots = 0;
        for card in &mut self.cards {
            let visible = self.active
                && !blocked
                && (!self.dnd || card.notice.urgency == 2)
                && slots < max_visible.clamp(1, 3);
            if visible {
                slots += 1;
            }
            if !card.closing && visible != card.visible {
                card.spring.jump(0.0);
                card.spring.set(if visible { 1.0 } else { 0.0 });
            }
            card.visible = visible;
            if !visible {
                card.rect = None;
            }
        }
        before
            != self
                .cards
                .iter()
                .map(|c| (c.notice.id, c.visible, c.closing))
                .collect::<Vec<_>>()
    }
    pub fn animating(&self) -> bool {
        !self.blocked && self.cards.iter().any(|c| c.visible && c.spring.is_active())
    }
    pub fn contains(&self, output: &Output, p: Point<f64, Logical>) -> bool {
        self.active
            && !self.blocked
            && self.output.as_ref() == Some(output)
            && self
                .cards
                .iter()
                .any(|c| c.visible && c.rect.is_some_and(|r| r.to_f64().contains(p)))
    }
    fn next_expiry(&self) -> Option<Duration> {
        self.cards
            .iter()
            .filter(|c| c.visible && !c.closing && c.hovered.is_none())
            .filter_map(|c| c.expires)
            .min()
    }
    fn hover(&mut self, local: Option<Point<f64, Logical>>, now: Duration) {
        for c in &mut self.cards {
            let hover = c.visible
                && !c.closing
                && local.is_some_and(|p| c.rect.is_some_and(|r| r.to_f64().contains(p)));
            match (hover, c.hovered) {
                (true, None) => c.hovered = Some(now),
                (false, Some(start)) => {
                    c.expires = c
                        .expires
                        .map(|t| t.saturating_add(now.saturating_sub(start)));
                    c.hovered = None;
                }
                _ => {}
            }
        }
    }
}

/// Also used by the harness: the production path has exactly this channel.
pub fn channel(
    handle: &smithay::reexports::calloop::LoopHandle<'static, Gavno>,
) -> Option<channel::Sender<Request>> {
    let (tx, rx) = channel::channel();
    match handle.insert_source(rx, |event, _, gv| {
        if let channel::Event::Msg(request) = event {
            gv.guard(Feature::Notifications, (), |gv| {
                let now = gv.start_time.elapsed();
                let mut state = gv.notifications.borrow_mut();
                match request {
                    Request::Active => state.active = true,
                    Request::Notify(n) => state.receive(n, now, gv.config.notifications.timeout_ms),
                    Request::Close(id) => state.close(id, None),
                }
                drop(state);
                gv.refresh_notifications();
                gv.queue_redraw_all();
            });
        }
    }) {
        Ok(_) => Some(tx),
        Err(error) => {
            tracing::warn!(%error, "cannot register notification channel");
            None
        }
    }
}

impl Gavno {
    pub fn refresh_notifications(&mut self) {
        self.guard(Feature::Notifications, (), |gv| {
            gv.refresh_notifications_inner(None)
        });
    }
    fn refresh_notifications_inner(&mut self, pointer: Option<Point<f64, Logical>>) {
        if !self.notifications.borrow().started
            && !matches!(self.backend, crate::backend::Backend::None)
            && self.greeter.is_none()
        {
            self.notifications.borrow_mut().started = true;
            if let Some(events) = channel(&self.loop_handle) {
                self.notifications.borrow_mut().signals = dbus::spawn(events);
            }
        }
        let output = self.active_output();
        let blocked = self.protocols.locks.locked
            || self.lock.internal()
            || self.greeter.is_some()
            || output.is_none();
        let now = self.start_time.elapsed();
        let pointer = pointer.unwrap_or_else(|| self.pointer_location());
        let mut state = self.notifications.borrow_mut();
        let moved = state.output != output;
        state.output = output;
        let changed = state.update(now, blocked, self.config.notifications.max_visible);
        if let Some(output) = &state.output {
            if let Some(geo) = self.space.output_geometry(output) {
                state.layout(geo.size);
            }
        }
        let local = state
            .output
            .as_ref()
            .and_then(|o| self.space.output_geometry(o))
            .map(|g| pointer - g.loc.to_f64());
        state.hover(local, now);
        let deadline = state.next_expiry();
        if state.timer.as_ref().map(|(d, _)| *d) != deadline {
            if let Some((_, token)) = state.timer.take() {
                self.loop_handle.remove(token);
            }
            if let Some(deadline) = deadline {
                match self.loop_handle.insert_source(
                    Timer::from_duration(deadline.saturating_sub(now)),
                    |_, _, gv| {
                        gv.guard(Feature::Notifications, (), |gv| {
                            gv.notifications.borrow_mut().timer = None;
                            gv.refresh_notifications_inner(None);
                        });
                        TimeoutAction::Drop
                    },
                ) {
                    Ok(token) => state.timer = Some((deadline, token)),
                    Err(error) => tracing::warn!(%error, "cannot schedule notification expiry"),
                }
            }
        }
        self.topbar.notifications_active = state.active;
        self.topbar.notification_history = state.history.iter().cloned().collect();
        self.topbar.notification_dnd = state.dnd;
        self.topbar.notification_unread = state.unread;
        drop(state);
        if changed || moved {
            self.queue_redraw_all();
        }
    }
    pub fn notification_motion(&mut self, pos: Point<f64, Logical>) {
        self.guard(Feature::Notifications, (), |gv| {
            gv.refresh_notifications_inner(Some(pos))
        });
    }
    pub fn notification_button(
        &mut self,
        pos: Point<f64, Logical>,
        button: u32,
        pressed: bool,
    ) -> bool {
        if self.protocols.locks.locked || self.lock.internal() || self.greeter.is_some() {
            return false;
        }
        self.guard(Feature::Notifications, false, |gv| {
            let mut state = gv.notifications.borrow_mut();
            let Some(output) = state.output.as_ref() else {
                return false;
            };
            let Some(geo) = gv.space.output_geometry(output) else {
                return false;
            };
            let p = pos - geo.loc.to_f64();
            let Some(c) = state
                .cards
                .iter()
                .find(|c| c.visible && c.rect.is_some_and(|r| r.to_f64().contains(p)))
            else {
                return false;
            };
            if pressed && button == 0x110 && !c.closing {
                let id = c.notice.id;
                let close = c
                    .rect
                    .is_some_and(|r| ui::close_rect(r).to_f64().contains(p));
                let action = if close {
                    None
                } else {
                    c.buttons
                        .iter()
                        .find(|(r, _)| r.to_f64().contains(p))
                        .map(|(_, k)| k.clone())
                        .or_else(|| {
                            c.notice
                                .actions
                                .iter()
                                .find(|(k, _)| k == "default")
                                .map(|(k, _)| k.clone())
                        })
                };
                if let Some(key) = action {
                    state.emit(Signal::Action(id, key));
                }
                state.close(id, Some(2));
            }
            drop(state);
            gv.refresh_notifications();
            gv.queue_redraw_all();
            true
        })
    }
    pub fn notification_history_action(&mut self, id: u32) {
        self.guard(Feature::Notifications, (), |gv| {
            let state = gv.notifications.borrow();
            if let Some(h) = state
                .history
                .iter()
                .find(|h| h.notice.id == id && h.notice.actions.iter().any(|(k, _)| k == "default"))
            {
                state.emit(Signal::HistoryAction(h.notice.id));
            }
        });
    }
    pub(crate) fn reset_notifications(&mut self) {
        let mut state = self.notifications.borrow_mut();
        if let Some((_, token)) = state.timer.take() {
            self.loop_handle.remove(token);
        }
        state.cards.clear();
        state.history.clear();
        state.unread = false;
        state.active = false;
        self.topbar.notifications_active = false;
        self.topbar.notification_history.clear();
        self.topbar.notification_unread = false;
    }
}

#[cfg(test)]
mod tests;
