//! A session-wide polkit agent, using the helper protocol from gavno-files.
mod protocol;

use adw::prelude::*;
use protocol::{Identity, AUTHORITY, AUTHORITY_INTERFACE, NAME, PATH};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::Arc,
};

const INTERFACE: &str = "org.freedesktop.PolicyKit1.AuthenticationAgent";
const CANCELLED: &str = "org.freedesktop.PolicyKit1.Error.Cancelled";
const FAILED: &str = "org.freedesktop.PolicyKit1.Error.Failed";
const XML: &str = r#"<node>
  <interface name="org.freedesktop.PolicyKit1.AuthenticationAgent">
    <method name="BeginAuthentication">
      <arg type="s" name="action_id" direction="in"/>
      <arg type="s" name="message" direction="in"/>
      <arg type="s" name="icon_name" direction="in"/>
      <arg type="a{ss}" name="details" direction="in"/>
      <arg type="s" name="cookie" direction="in"/>
      <arg type="a(sa{sv})" name="identities" direction="in"/>
    </method>
    <method name="CancelAuthentication">
      <arg type="s" name="cookie" direction="in"/>
    </method>
  </interface>
</node>"#;

type Requests = Rc<RefCell<HashMap<String, Rc<Request>>>>;
type Begin = (
    String,
    String,
    String,
    HashMap<String, String>,
    String,
    Vec<Identity>,
);

struct Agent {
    connection: gio::DBusConnection,
    registration: gio::RegistrationId,
    session: String,
    requests: Requests,
}

impl Agent {
    fn register(app: &adw::Application) -> Result<Self, String> {
        let connection = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)
            .map_err(|e| e.to_string())?;
        let session = protocol::session_id(&connection)?;
        let reply = connection
            .call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                Some(&(NAME,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                -1,
                gio::Cancellable::NONE,
            )
            .map_err(|e| e.to_string())?;
        let (owner,) = reply
            .get::<(String,)>()
            .filter(|(name,)| name.starts_with(':'))
            .ok_or("Invalid polkitd unique name")?;
        let interface = gio::DBusNodeInfo::for_xml(XML)
            .map_err(|e| e.to_string())?
            .lookup_interface(INTERFACE)
            .ok_or("Missing authentication interface")?;
        let requests = Requests::default();
        let active = requests.clone();
        let weak_app = app.downgrade();
        let registration = connection
            .register_object(PATH, &interface)
            .method_call(move |_, sender, _, _, method, params, invocation| {
                if sender != Some(owner.as_str()) {
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.AccessDenied",
                        "Not polkitd",
                    );
                    return;
                }
                match method {
                    "BeginAuthentication" => {
                        let Some(args) = params.get::<Begin>() else {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "Invalid authentication request",
                            );
                            return;
                        };
                        let Some(app) = weak_app.upgrade() else {
                            invocation.return_dbus_error(FAILED, "Application is shutting down");
                            return;
                        };
                        Request::open(&app, &active, args, invocation);
                    }
                    "CancelAuthentication" => {
                        let Some((cookie,)) = params.get::<(String,)>() else {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "Invalid cookie",
                            );
                            return;
                        };
                        let request = active.borrow().get(&cookie).cloned();
                        if let Some(request) = request {
                            request.finish(Some((CANCELLED, "Cancelled")));
                        }
                        invocation.return_value(None);
                    }
                    _ => invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        "Unknown method",
                    ),
                }
            })
            .build()
            .map_err(|e| e.to_string())?;
        let locale = std::env::var("LANG").unwrap_or_else(|_| "C".into());
        if let Err(error) = connection.call_sync(
            Some(NAME),
            AUTHORITY,
            AUTHORITY_INTERFACE,
            "RegisterAuthenticationAgent",
            Some(&protocol::registration_params(&session, &locale)),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
        ) {
            let _ = connection.unregister_object(registration);
            if already_registered(&error) {
                return Err("already-registered".into());
            }
            return Err(error.to_string());
        }
        Ok(Self {
            connection,
            registration,
            session,
            requests,
        })
    }

    fn stop(self) {
        let requests: Vec<_> = self.requests.borrow().values().cloned().collect();
        for request in requests {
            request.finish(Some((CANCELLED, "Agent is shutting down")));
        }
        let params =
            glib::Variant::tuple_from_iter([protocol::subject(&self.session), PATH.to_variant()]);
        if let Err(error) = self.connection.call_sync(
            Some(NAME),
            AUTHORITY,
            AUTHORITY_INTERFACE,
            "UnregisterAuthenticationAgent",
            Some(&params),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
        ) {
            eprintln!("gavno-polkit-agent: cannot unregister: {error}");
        }
        let _ = self.connection.unregister_object(self.registration);
    }
}

fn already_registered(error: &glib::Error) -> bool {
    let message = error.message().to_ascii_lowercase();
    gio::DBusError::remote_error(error).as_deref() == Some(FAILED)
        && (message.contains("authentication agent already exists")
            || message.contains("authentication agent is already registered"))
}

struct Request {
    cookie: String,
    requests: std::rc::Weak<RefCell<HashMap<String, Rc<Request>>>>,
    invocation: RefCell<Option<gio::DBusMethodInvocation>>,
    window: adw::Window,
    entry: gtk::PasswordEntry,
    users: Vec<String>,
    choice: gtk::DropDown,
    authenticate: gtk::Button,
    error: gtk::Label,
    attempts: Cell<u8>,
    busy: Cell<bool>,
    helper: RefCell<Option<Arc<protocol::Helper>>>,
}

impl Request {
    fn open(
        app: &adw::Application,
        requests: &Requests,
        args: Begin,
        invocation: gio::DBusMethodInvocation,
    ) {
        let (action, message, icon, details, cookie, identities) = args;
        if requests.borrow().contains_key(&cookie) {
            invocation.return_dbus_error(FAILED, "Duplicate authentication cookie");
            return;
        }
        let users: Vec<_> = protocol::allowed_uids(&identities)
            .into_iter()
            .filter_map(|uid| protocol::user_name(uid).map(|name| (uid, name)))
            .collect();
        let uids: Vec<_> = users.iter().map(|(uid, _)| *uid).collect();
        let Some(selected) = protocol::preferred_user(&uids, unsafe { libc::getuid() }) else {
            invocation.return_dbus_error(FAILED, "No supported user to authenticate as");
            return;
        };
        let users: Vec<_> = users.into_iter().map(|(_, name)| name).collect();
        let window = adw::Window::builder()
            .application(app)
            .title("Authentication Required")
            .default_width(440)
            .resizable(false)
            .build();
        window.add_css_class("polkit");
        let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
        for set in [
            gtk::Widget::set_margin_top,
            gtk::Widget::set_margin_bottom,
            gtk::Widget::set_margin_start,
            gtk::Widget::set_margin_end,
        ] {
            set(body.upcast_ref(), 24);
        }
        let icon_name = gtk::IconTheme::for_display(&WidgetExt::display(&window));
        let icon = if !icon.is_empty() && icon_name.has_icon(&icon) {
            &icon
        } else {
            "dialog-password-symbolic"
        };
        let image = gtk::Image::from_icon_name(icon);
        image.set_pixel_size(48);
        body.append(&image);
        let heading = gtk::Label::new(Some("Authentication Required"));
        heading.add_css_class("title-2");
        body.append(&heading);
        let label = gtk::Label::new(Some(&message));
        label.set_wrap(true);
        label.set_max_width_chars(48);
        body.append(&label);
        let names: Vec<_> = users.iter().map(String::as_str).collect();
        let choice = gtk::DropDown::from_strings(&names);
        choice.set_selected(selected as u32);
        choice.set_visible(users.len() > 1);
        body.append(&choice);
        let entry = gtk::PasswordEntry::new();
        entry.set_show_peek_icon(true);
        entry.set_placeholder_text(Some(&format!("Password for {}", users[selected])));
        entry.set_activates_default(true);
        body.append(&entry);
        let error = gtk::Label::new(None);
        error.add_css_class("error");
        error.set_wrap(true);
        error.set_visible(false);
        body.append(&error);
        let expander = gtk::Expander::new(Some("Details"));
        let mut details: Vec<_> = details.into_iter().collect();
        details.sort_by(|a, b| a.0.cmp(&b.0));
        let mut text = format!("Action: {action}");
        for (key, value) in details {
            text.push_str(&format!("\n{key}: {value}"));
        }
        let detail_label = gtk::Label::new(Some(&text));
        detail_label.set_wrap(true);
        detail_label.set_selectable(true);
        detail_label.set_xalign(0.0);
        detail_label.set_max_width_chars(48);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&detail_label)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(180)
            .propagate_natural_height(true)
            .build();
        expander.set_child(Some(&scroll));
        body.append(&expander);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let authenticate = gtk::Button::with_label("Authenticate");
        authenticate.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&authenticate);
        body.append(&buttons);
        window.set_content(Some(&body));
        window.set_default_widget(Some(&authenticate));
        let request = Rc::new(Self {
            cookie: cookie.clone(),
            requests: Rc::downgrade(requests),
            invocation: RefCell::new(Some(invocation)),
            window,
            entry,
            users,
            choice,
            authenticate,
            error,
            attempts: Cell::new(0),
            busy: Cell::new(false),
            helper: RefCell::new(None),
        });
        requests.borrow_mut().insert(cookie, request.clone());
        let weak = Rc::downgrade(&request);
        request.authenticate.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.submit();
            }
        });
        let weak = Rc::downgrade(&request);
        cancel.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.finish(Some((CANCELLED, "Cancelled")));
            }
        });
        let weak = Rc::downgrade(&request);
        request.window.connect_close_request(move |_| {
            if let Some(r) = weak.upgrade() {
                r.finish(Some((CANCELLED, "Cancelled")));
            }
            glib::Propagation::Stop
        });
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&request);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if let Some(r) = weak.upgrade() {
                    r.finish(Some((CANCELLED, "Cancelled")));
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        request.window.add_controller(keys);
        let weak = Rc::downgrade(&request);
        request.choice.connect_selected_notify(move |choice| {
            if let Some(r) = weak.upgrade() {
                r.entry.set_text("");
                if let Some(user) = r.users.get(choice.selected() as usize) {
                    r.entry
                        .set_placeholder_text(Some(&format!("Password for {user}")));
                }
                r.entry.grab_focus();
            }
        });
        request.window.present();
        request.entry.grab_focus();
    }

    fn finish(&self, error: Option<(&str, &str)>) {
        let Some(invocation) = self.invocation.borrow_mut().take() else {
            return;
        };
        if let Some(helper) = self.helper.borrow_mut().take() {
            helper.cancel();
        }
        self.entry.set_text("");
        if let Some(requests) = self.requests.upgrade() {
            requests.borrow_mut().remove(&self.cookie);
        }
        self.window.destroy();
        match error {
            Some((name, message)) => invocation.return_dbus_error(name, message),
            None => invocation.return_value(None),
        }
    }

    fn submit(self: &Rc<Self>) {
        if self.busy.get() || self.invocation.borrow().is_none() {
            return;
        }
        let Some(user) = self.users.get(self.choice.selected() as usize).cloned() else {
            return;
        };
        let password = self.entry.text().to_string();
        self.entry.set_text("");
        self.busy.set(true);
        self.attempts.set(self.attempts.get() + 1);
        self.entry.set_sensitive(false);
        self.choice.set_sensitive(false);
        self.authenticate.set_sensitive(false);
        self.error.set_visible(false);
        let helper = Arc::new(protocol::Helper::default());
        self.helper.replace(Some(helper.clone()));
        let cookie = self.cookie.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result =
                gio::spawn_blocking(move || helper.authenticate(&user, &cookie, &password)).await;
            let Some(request) = weak.upgrade() else {
                return;
            };
            if request.invocation.borrow().is_none() {
                return;
            }
            request.helper.take();
            let message = match result {
                Ok(Ok(true)) => {
                    request.finish(None);
                    return;
                }
                Ok(Ok(false)) => "Wrong password, try again".into(),
                Ok(Err(error)) => error,
                Err(_) => "The password check failed".into(),
            };
            if request.attempts.get() >= 3 {
                request.finish(Some((FAILED, "Authentication failed after three attempts")));
                return;
            }
            request.error.set_text(&message);
            request.error.set_visible(true);
            request.busy.set(false);
            request.entry.set_sensitive(true);
            request.choice.set_sensitive(true);
            request.authenticate.set_sensitive(true);
            request.entry.grab_focus();
        });
    }
}

/// GTK's software renderer for this small, mostly static window: it starts
/// faster and keeps about 100 MB less than the GL or Vulkan renderers
/// (measured on the Snapdragon laptop). An explicit GSK_RENDERER still wins.
fn prefer_software_rendering() {
    if std::env::var_os("GSK_RENDERER").is_none() {
        std::env::set_var("GSK_RENDERER", "cairo");
    }
}

/// Cap glibc malloc at two arenas. Every helper thread (Mesa shader queues,
/// D-Bus workers) otherwise gets its own arena; in the compositor that was
/// ~58 MB of idle RSS. An explicit MALLOC_ARENA_MAX still wins.
fn limit_malloc_arenas() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        // SAFETY: mallopt only adjusts allocator tuning; called before any thread starts.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, 2);
        }
    }
}

fn main() -> glib::ExitCode {
    limit_malloc_arenas();
    prefer_software_rendering();
    let app = adw::Application::builder()
        .application_id("org.gavno.PolkitAgent")
        .build();
    let agent = Rc::new(RefCell::new(None::<Agent>));
    let hold = Rc::new(RefCell::new(None));
    let failed = Rc::new(Cell::new(false));
    let state = agent.clone();
    let keep_alive = hold.clone();
    let startup_failed = failed.clone();
    app.connect_startup(move |app| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("style.css"));
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        match Agent::register(app) {
            Ok(agent) => { state.replace(Some(agent)); keep_alive.replace(Some(app.hold())); }
            Err(error) => {
                if error == "already-registered" {
                    eprintln!("gavno-polkit-agent: another authentication agent is registered for this session");
                } else {
                    eprintln!("gavno-polkit-agent: cannot register: {error}");
                    startup_failed.set(true);
                }
                app.quit();
            }
        }
    });
    // Activation deliberately leaves the application windowless until polkit asks.
    app.connect_activate(|_| {});
    let state = agent.clone();
    app.connect_shutdown(move |_| {
        if let Some(agent) = state.take() {
            agent.stop();
        }
    });
    let mut signals = Vec::new();
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let weak = app.downgrade();
        signals.push(glib::unix_signal_add_local(signal, move || {
            if let Some(app) = weak.upgrade() {
                app.quit();
            }
            glib::ControlFlow::Continue
        }));
    }
    let result = app.run();
    for signal in signals {
        signal.remove();
    }
    hold.take();
    if failed.get() {
        glib::ExitCode::FAILURE
    } else {
        result
    }
}
