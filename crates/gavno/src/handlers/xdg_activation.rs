use crate::Gavno;
use smithay::{
    input::Seat,
    reexports::wayland_server::{protocol::wl_surface::WlSurface, Resource},
    wayland::xdg_activation::{
        XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
    },
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};

/// Stored on the window so a future taskbar can display attention requests.
#[derive(Default)]
pub struct Urgent(pub AtomicBool);
const TOKEN_LIFETIME: Duration = Duration::from_secs(5);

/// A consumed token can authorize a later first map. Keep that decision on
/// the surface, independently of both the token and launch measurements.
struct FocusAuthorization {
    epoch: u64,
    allowed: bool,
}
impl XdgActivationHandler for Gavno {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.protocols.activation
    }
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        if self.greeter.is_some() || self.protocols.locks.locked {
            return false;
        }
        let Some((serial, seat)) = data.serial else {
            return false;
        };
        if Seat::<Self>::from_resource(&seat).as_ref() != Some(&self.seat) {
            return false;
        }
        let Some(keyboard) = self.seat.get_keyboard() else {
            return false;
        };
        let Some(focus) = keyboard.current_focus() else {
            return false;
        };
        let owned = data
            .client_id
            .as_ref()
            .is_some_and(|id| focus.client().is_some_and(|c| c.id() == *id));
        owned
            && self
                .protocols
                .activation_serials
                .iter()
                .any(|(issued, client, time)| {
                    *issued == serial
                        && Some(client) == data.client_id.as_ref()
                        && time.elapsed() <= TOKEN_LIFETIME
                })
            && data.timestamp.elapsed() <= TOKEN_LIFETIME
    }
    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let launch_allowed = self.consume_launch_token(&token);
        self.launch_activation(token.as_str(), &surface);
        self.protocols.activation.remove_token(&token);
        let window = self.window_for_surface(&surface).or_else(|| {
            self.unmapped
                .iter()
                .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &surface))
                .cloned()
        });
        let Some(window) = window else { return };
        if !self.protocols.locks.locked
            && self.greeter.is_none()
            && !self.polkit_visible()
            && self
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus())
                .and_then(|focused| self.window_for_keyboard_surface(&focused))
                .as_ref()
                == Some(&window)
        {
            // A late/revoked request cannot demand attention from the window
            // the user already chose. Token consumption above still applies.
            clear_urgency(&window);
            return;
        }
        let allowed = launch_allowed.unwrap_or_else(|| data.timestamp.elapsed() <= TOKEN_LIFETIME)
            && !self.protocols.locks.locked
            && self.greeter.is_none()
            && !self.polkit_visible();
        if self.space.element_location(&window).is_none() {
            smithay::wayland::compositor::with_states(&surface, |states| {
                states
                    .data_map
                    .insert_if_missing_threadsafe(|| Mutex::new(None::<FocusAuthorization>));
                *states
                    .data_map
                    .get::<Mutex<Option<FocusAuthorization>>>()
                    .unwrap()
                    .lock()
                    .unwrap() = Some(FocusAuthorization {
                    epoch: self.launch_focus_epoch(),
                    allowed,
                });
            });
        }
        if allowed {
            clear_urgency(&window);
            if self.space.element_location(&window).is_some() {
                self.focus_window(Some(&window));
            }
        } else {
            mark_urgent(&window);
            self.queue_redraw_all();
        }
    }
}
smithay::delegate_xdg_activation!(Gavno);

impl Gavno {
    /// Popup and subsurface focus belongs to the same attention target as the
    /// toplevel. Do not change general window lookup semantics for other input.
    pub(crate) fn window_for_keyboard_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<smithay::desktop::Window> {
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        if let Some(popup) = self.popups.find_popup(&root) {
            root = smithay::desktop::find_popup_root_surface(&popup).ok()?;
        }
        self.window_for_surface(&root)
    }
    pub(crate) fn first_map_focus_allowed(&self, window: &smithay::desktop::Window) -> bool {
        let authorization = window.toplevel().and_then(|toplevel| {
            smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                states
                    .data_map
                    .get::<Mutex<Option<FocusAuthorization>>>()
                    .and_then(|authorization| authorization.lock().unwrap().take())
            })
        });
        if self.polkit_visible()
            || authorization.is_some_and(|authorization| {
                !authorization.allowed || authorization.epoch != self.launch_focus_epoch()
            })
        {
            mark_urgent(window);
        }
        let allowed = !is_urgent(window) && !self.protocols.locks.locked && self.greeter.is_none();
        if !allowed {
            window.set_activated(false);
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_pending_configure();
            }
        }
        allowed
    }
    pub fn record_activation_serial(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
    ) {
        let Some(client) = surface.and_then(|s| s.client()) else {
            return;
        };
        self.protocols.activation_serials.push_back((
            serial,
            client.id(),
            std::time::Instant::now(),
        ));
        while self.protocols.activation_serials.len() > 64 {
            self.protocols.activation_serials.pop_front();
        }
    }
}

pub fn is_urgent(window: &smithay::desktop::Window) -> bool {
    window
        .user_data()
        .get::<Urgent>()
        .is_some_and(|urgent| urgent.0.load(Ordering::Relaxed))
}
pub fn clear_urgency(window: &smithay::desktop::Window) {
    if let Some(urgent) = window.user_data().get::<Urgent>() {
        urgent.0.store(false, Ordering::Relaxed);
    }
}

pub(crate) fn mark_urgent(window: &smithay::desktop::Window) {
    window
        .user_data()
        .insert_if_missing_threadsafe(Urgent::default);
    window
        .user_data()
        .get::<Urgent>()
        .unwrap()
        .0
        .store(true, Ordering::Relaxed);
}
