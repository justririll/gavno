use crate::Gavno;
use smithay::{
    input::Seat,
    reexports::wayland_server::{protocol::wl_surface::WlSurface, Resource},
    wayland::xdg_activation::{
        XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
    },
};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// Stored on the window so a future taskbar can display attention requests.
#[derive(Default)]
pub struct Urgent(pub AtomicBool);
const TOKEN_LIFETIME: Duration = Duration::from_secs(5);
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
        self.protocols.activation.remove_token(&token);
        let window = self.window_for_surface(&surface).or_else(|| {
            self.unmapped
                .iter()
                .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &surface))
                .cloned()
        });
        let Some(window) = window else { return };
        if data.timestamp.elapsed() <= TOKEN_LIFETIME
            && !self.protocols.locks.locked
            && self.greeter.is_none()
        {
            if self.space.element_location(&window).is_some() {
                self.focus_window(Some(&window));
            }
        } else {
            window
                .user_data()
                .insert_if_missing_threadsafe(Urgent::default);
            if let Some(urgent) = window.user_data().get::<Urgent>() {
                urgent.0.store(true, Ordering::Relaxed);
            }
            self.queue_redraw_all();
        }
    }
}
smithay::delegate_xdg_activation!(Gavno);

impl Gavno {
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
