//! Notification state belongs to the event loop; D-Bus never borrows compositor state.
pub mod dbus;
mod ui;
#[cfg(test)]
pub(crate) use ui::kernel_line;

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
use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

pub type Shared = Rc<RefCell<Notifications>>;

/// How long a notification line stays in TTY mode.
pub(crate) const TTY_LIFETIME: Duration = Duration::from_secs(4);

#[derive(Clone, Debug)]
pub struct Notice {
    pub id: u32,
    pub gen: u64,
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
    Close(u32, u64),
    Active,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Signal {
    Closed(u32, u64, u32),
    Action(u32, u64, String),
}

struct Card {
    notice: Notice,
    ttl: Option<Duration>,
    expires: Option<Duration>,
    hovered: Option<Duration>,
    visible: bool,
    closing: bool,
    spring: Spring,
    rect: Option<Rect>,
    buttons: Vec<(Rect, String)>,
    /// Body wrapped for a panel width; recomputed when either changes.
    lines: Option<(i32, Vec<String>)>,
    /// Arrival time, the kernel-style stamp of TTY mode.
    born: Duration,
}
#[derive(Default)]
pub struct Notifications {
    cards: Vec<Card>,
    pub history: VecDeque<History>,
    pub unread: bool,
    pub dnd: bool,
    /// Bumped on every history change, so the bar copy is refreshed only
    /// when needed instead of on every input event.
    history_rev: u64,
    synced_rev: Option<u64>,
    /// Lowest battery threshold already announced while discharging.
    battery_warned: Option<u8>,
    enabled: Arc<AtomicBool>,
    registry: dbus::Live,
    active: bool,
    started: bool,
    blocked: bool,
    /// TTY mode: kernel message lines that go away after `TTY_LIFETIME`.
    tty: bool,
    /// Area of the open bar popover (set by the renderer); toasts keep clear of it.
    pub(crate) avoid: Option<Rect>,
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
        let Some(end) = rest.find('>') else {
            break;
        };
        let tag = rest[1..end].trim();
        if !(matches!(tag, "b" | "/b" | "i" | "/i" | "u" | "/u" | "a" | "/a")
            || tag.starts_with("a "))
        {
            out.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
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
        let clock = crate::ui::theme::LocalTime::now();
        let history = History {
            notice: n.clone(),
            time: format!("{:02}:{:02}", clock.hour, clock.minute),
        };
        self.history.retain(|h| h.notice.id != n.id);
        self.history.push_front(history);
        self.history.truncate(50);
        self.history_rev += 1;
        self.unread = true;
        if let Some(card) = self
            .cards
            .iter_mut()
            .find(|c| c.notice.id == n.id && !c.closing)
        {
            card.notice = n;
            card.lines = None;
            card.ttl = ttl;
            card.expires = ttl.filter(|_| card.visible).map(|t| now.saturating_add(t));
            card.hovered = card.hovered.map(|_| now);
        } else {
            if self.cards.iter().filter(|c| !c.closing).count() >= 32 {
                let victim = self
                    .cards
                    .iter()
                    .find(|c| !c.closing && !c.visible && c.notice.urgency != 2)
                    .or_else(|| self.cards.iter().find(|c| !c.closing && !c.visible))
                    .map(|c| c.notice.id);
                if let Some(id) = victim {
                    self.close(id, Some(4));
                }
            }
            self.cards.push(Card {
                notice: n,
                ttl,
                expires: None,
                hovered: None,
                visible: false,
                closing: false,
                spring: Spring::new(0.0, SpringParams::SNAPPY),
                rect: None,
                buttons: Vec::new(),
                lines: None,
                born: now,
            });
        }
    }
    pub fn clear_history(&mut self) {
        self.history.clear();
        self.history_rev += 1;
        self.unread = false;
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
            let gen = card.notice.gen;
            if let Some(reason) = reason {
                self.emit(Signal::Closed(id, gen, reason));
            }
        }
    }
    fn update(&mut self, now: Duration, blocked: bool, max_visible: usize) -> bool {
        let before: Vec<_> = self
            .cards
            .iter()
            .map(|c| (c.notice.id, c.visible, c.closing))
            .collect();
        // Pause newly hidden cards before checking their deadlines.
        self.set_visibility(now, blocked, max_visible);
        let expired: Vec<_> = self
            .cards
            .iter()
            .filter(|c| {
                c.visible
                    && !c.closing
                    && c.hovered.is_none()
                    && c.expires.is_some_and(|t| t <= now)
            })
            .map(|c| c.notice.id)
            .collect();
        for id in expired {
            self.close(id, Some(1));
        }
        self.cards
            .retain(|c| !(c.closing && (!c.visible || !c.spring.is_active())));
        self.blocked = blocked;
        // Finished closing animations may have freed a visible slot.
        self.set_visibility(now, blocked, max_visible);
        before
            != self
                .cards
                .iter()
                .map(|c| (c.notice.id, c.visible, c.closing))
                .collect::<Vec<_>>()
    }
    /// Switches the look; the cards stay, so nothing is lost either way.
    pub(crate) fn set_tty(&mut self, on: bool) {
        self.tty = on;
    }
    fn set_visibility(&mut self, now: Duration, blocked: bool, max_visible: usize) {
        let tty = self.tty;
        let mut slots = 0;
        for card in &mut self.cards {
            let visible = self.active
                && !blocked
                && (!self.dnd || card.notice.urgency == 2)
                && slots < max_visible.clamp(1, 3);
            if visible {
                slots += 1;
            }
            if visible != card.visible {
                if visible {
                    // Kernel messages scroll away: nothing stays beyond four seconds.
                    let ttl = if tty { Some(card.ttl.map_or(TTY_LIFETIME, |t| t.min(TTY_LIFETIME))) } else { card.ttl };
                    card.expires = ttl.map(|ttl| now.saturating_add(ttl));
                } else {
                    if let Some(start) = card.hovered.take() {
                        card.expires = card
                            .expires
                            .map(|t| t.saturating_add(now.saturating_sub(start)));
                    }
                    card.ttl = card.expires.take().map(|t| t.saturating_sub(now));
                }
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
    }
    fn history_action(&mut self, id: u32) {
        let Some(h) = self.history.iter().find(|h| {
            h.notice.id == id && h.notice.actions.iter().any(|(key, _)| key == "default")
        }) else {
            return;
        };
        let gen = h.notice.gen;
        let live = self.registry.lock().is_ok_and(|registry| {
            registry
                .live
                .get(&id)
                .is_some_and(|(_, current)| *current == gen)
        });
        if live {
            self.emit(Signal::Action(id, gen, "default".into()));
            self.close(id, Some(2));
        }
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

/// Ids for GAVNO's own notifications. D-Bus ids count up from 1 and never
/// get near these, and the registry does not know them, so their close and
/// action signals are dropped.
const BATTERY_ID: u32 = u32::MAX - 1;

/// Battery thresholds: the warning to show when a discharging battery drops
/// to `percent` or below, as (threshold, urgency).
const BATTERY_STEPS: [(u8, u8); 2] = [(5, 2), (15, 1)];

/// The warning to show for this reading, if it crosses a threshold that was
/// not announced yet. `None` from `warned` means nothing was shown.
pub(crate) fn battery_warning(
    battery: Option<&crate::topbar::Battery>,
    warned: Option<u8>,
) -> Option<(u8, u8)> {
    let battery = battery.filter(|b| !b.charging)?;
    BATTERY_STEPS
        .into_iter()
        .find(|(threshold, _)| battery.percent <= *threshold)
        .filter(|(threshold, _)| warned.is_none_or(|w| *threshold < w))
}

/// Also used by the harness: the production path has exactly this channel.
pub fn channel(
    handle: &smithay::reexports::calloop::LoopHandle<'static, Gavno>,
) -> Option<channel::Sender<Request>> {
    let (tx, rx) = channel::channel();
    match handle.insert_source(rx, |event, _, gv| {
        let _stall = crate::perf::Stall::new("dbus.notifications");
        if let channel::Event::Msg(request) = event {
            gv.guard(Feature::Notifications, (), |gv| {
                let now = gv.start_time.elapsed();
                let mut state = gv.notifications.borrow_mut();
                match request {
                    Request::Active => state.active = true,
                    Request::Notify(n) => state.receive(n, now, gv.config.notifications.timeout_ms),
                    Request::Close(id, gen) => {
                        if state
                            .cards
                            .iter()
                            .any(|c| c.notice.id == id && c.notice.gen == gen && !c.closing)
                        {
                            state.close(id, None);
                        }
                    }
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
        self.notifications
            .borrow()
            .enabled
            .store(true, Ordering::Release);
        if !self.notifications.borrow().started
            && !matches!(self.backend, crate::backend::Backend::None)
            && self.greeter.is_none()
        {
            self.notifications.borrow_mut().started = true;
            let _startup = crate::perf::Startup::new("dbus.notifications.spawn.begin", "dbus.notifications.spawn.end");
            if let Some(events) = channel(&self.loop_handle) {
                let mut state = self.notifications.borrow_mut();
                state.signals = dbus::spawn(events, state.registry.clone(), state.enabled.clone());
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
                        let _stall = crate::perf::Stall::new("timer.notification_expiry");
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
        if state.synced_rev != Some(state.history_rev) {
            state.synced_rev = Some(state.history_rev);
            self.topbar.notification_history = state.history.iter().cloned().collect();
        }
        self.topbar.notification_dnd = state.dnd;
        self.topbar.notification_unread = state.unread;
        drop(state);
        if changed || moved {
            self.queue_redraw_all();
        }
    }
    /// Warns once per threshold while the battery discharges, and takes the
    /// warning down when a charger is plugged in.
    pub fn battery_notification(&mut self, battery: Option<&crate::topbar::Battery>) {
        self.guard(Feature::Notifications, (), |gv| {
            let now = gv.start_time.elapsed();
            let mut state = gv.notifications.borrow_mut();
            if battery.is_none_or(|b| b.charging) {
                if state.battery_warned.take().is_some() {
                    state.close(BATTERY_ID, None);
                }
                return;
            }
            let Some((threshold, urgency)) = battery_warning(battery, state.battery_warned) else {
                return;
            };
            state.battery_warned = Some(threshold);
            let percent = battery.map_or(threshold, |b| b.percent);
            let notice = Notice {
                id: BATTERY_ID,
                gen: 0,
                app: "Battery".into(),
                icon: format!("battery-level-{}-symbolic", (percent / 10) * 10),
                desktop: String::new(),
                summary: if urgency == 2 { "Battery critically low".into() } else { "Battery low".into() },
                body: if urgency == 2 {
                    format!("{percent}% left. Plug in the charger now.")
                } else {
                    format!("{percent}% left.")
                },
                actions: Vec::new(),
                urgency,
                timeout: -1,
            };
            state.receive(notice, now, gv.config.notifications.timeout_ms);
            drop(state);
            gv.refresh_notifications();
            gv.queue_redraw_all();
        });
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
                let gen = c.notice.gen;
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
                    state.emit(Signal::Action(id, gen, key));
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
            gv.notifications.borrow_mut().history_action(id);
            gv.refresh_notifications();
        });
    }
    pub(crate) fn reset_notifications(&mut self) {
        let mut state = self.notifications.borrow_mut();
        if let Some((_, token)) = state.timer.take() {
            self.loop_handle.remove(token);
        }
        state.enabled.store(false, Ordering::Release);
        for card in &state.cards {
            if !card.closing {
                state.emit(Signal::Closed(card.notice.id, card.notice.gen, 4));
            }
        }
        state.cards.clear();
        state.clear_history();
        self.topbar.notifications_active = false;
        self.topbar.notification_history.clear();
        self.topbar.notification_unread = false;
    }
}

#[cfg(test)]
mod tests;
