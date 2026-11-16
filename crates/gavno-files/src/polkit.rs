//! A polkit authentication agent for this process only, like
//! `pkttyagent --process`. GAVNO runs no session-wide agent, so root mode
//! registers this one before it starts `pkexec`; polkitd then asks it for
//! the password, and `polkit-agent-helper-1` checks it through PAM.
use adw::prelude::*;
use std::{
    cell::RefCell,
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

const PATH: &str = "/org/gavno/Files/PolkitAgent";
const NAME: &str = "org.freedesktop.PolicyKit1";
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
const HELPERS: [&str; 3] = [
    "/usr/lib/polkit-1/polkit-agent-helper-1",
    "/usr/libexec/polkit-agent-helper-1",
    "/usr/lib/polkit/polkit-agent-helper-1",
];

thread_local! {
    static AGENT: RefCell<Option<(gio::DBusConnection, gio::RegistrationId)>> = const { RefCell::new(None) };
    static DIALOGS: RefCell<HashMap<String, adw::AlertDialog>> = RefCell::new(HashMap::new());
}

/// Registers the agent once per process.
pub fn ensure(app: &adw::Application) -> Result<(), String> {
    if AGENT.with(|a| a.borrow().is_some()) {
        return Ok(());
    }
    let registered = register(app).map_err(|e| format!("Cannot start the password prompt: {e}"))?;
    AGENT.with(|a| a.replace(Some(registered)));
    Ok(())
}

fn register(app: &adw::Application) -> Result<(gio::DBusConnection, gio::RegistrationId), glib::Error> {
    let connection = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)?;
    // Only polkitd may drive the agent.
    let owner: String = connection
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            Some(&(NAME,).to_variant()),
            Some(glib::VariantTy::new("(s)").unwrap()),
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
        )?
        .child_value(0)
        .get()
        .unwrap_or_default();
    let node = gio::DBusNodeInfo::for_xml(XML)?;
    let interface = node
        .lookup_interface("org.freedesktop.PolicyKit1.AuthenticationAgent")
        .expect("interface in XML");
    let app = app.downgrade();
    let id = connection
        .register_object(PATH, &interface)
        .method_call(move |_, sender, _, _, method, params, invocation| {
            if sender != Some(owner.as_str()) {
                invocation.return_dbus_error("org.freedesktop.DBus.Error.AccessDenied", "not polkitd");
                return;
            }
            match method {
                "BeginAuthentication" => {
                    let Some(app) = app.upgrade() else {
                        invocation.return_dbus_error(
                            "org.freedesktop.PolicyKit1.Error.Failed",
                            "application is gone",
                        );
                        return;
                    };
                    glib::spawn_future_local(begin(app, params, invocation));
                }
                "CancelAuthentication" => {
                    let cookie: String = params.child_value(0).get().unwrap_or_default();
                    if let Some(dialog) = DIALOGS.with(|d| d.borrow_mut().remove(&cookie)) {
                        dialog.force_close();
                    }
                    invocation.return_value(None);
                }
                _ => invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    "unknown method",
                ),
            }
        })
        .build()?;
    let locale = std::env::var("LANG").unwrap_or_else(|_| "C".into());
    let text = format!(
        "(('unix-process', {{'pid': <uint32 {}>, 'start-time': <uint64 {}>}}), '{}', '{}')",
        std::process::id(),
        start_time(),
        locale.replace(['\'', '\\'], ""),
        PATH
    );
    let params = glib::Variant::parse(Some(glib::VariantTy::new("((sa{sv})ss)").unwrap()), &text)?;
    if let Err(e) = connection.call_sync(
        Some(NAME),
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
        "RegisterAuthenticationAgent",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        gio::Cancellable::NONE,
    ) {
        connection.unregister_object(id).ok();
        return Err(e);
    }
    Ok((connection, id))
}

/// Process start time in clock ticks, field 22 of /proc/self/stat.
fn start_time() -> u64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|stat| {
            let rest = stat.rsplit_once(')')?.1;
            rest.split_whitespace().nth(19)?.parse().ok()
        })
        .unwrap_or(0)
}

/// The user to authenticate as: ourselves when allowed, else the first
/// administrator polkit offers.
fn pick_user(identities: &glib::Variant) -> Option<(u32, String)> {
    let mut uids = Vec::new();
    for i in 0..identities.n_children() {
        let identity = identities.child_value(i);
        let kind: String = identity.child_value(0).get().unwrap_or_default();
        if kind != "unix-user" {
            continue;
        }
        let dict = glib::VariantDict::new(Some(&identity.child_value(1)));
        if let Some(uid) = dict.lookup_value("uid", None).and_then(|v| v.get::<u32>()) {
            uids.push(uid);
        }
    }
    let own = unsafe { libc::getuid() };
    let uid = if uids.contains(&own) { own } else { *uids.first()? };
    user_name(uid).map(|name| (uid, name))
}

fn user_name(uid: u32) -> Option<String> {
    let mut buffer = vec![0u8; 4096];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr().cast(), buffer.len(), &mut result)
    };
    if status != 0 || result.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

async fn begin(app: adw::Application, params: glib::Variant, invocation: gio::DBusMethodInvocation) {
    let message: String = params.child_value(1).get().unwrap_or_default();
    let cookie: String = params.child_value(4).get().unwrap_or_default();
    let Some((_, user)) = pick_user(&params.child_value(5)) else {
        invocation.return_dbus_error("org.freedesktop.PolicyKit1.Error.Failed", "no user to authenticate as");
        return;
    };
    let mut error = None::<String>;
    for _ in 0..3 {
        let Some(password) = ask(&app, &message, &user, &cookie, error.as_deref()).await else {
            invocation.return_dbus_error("org.freedesktop.PolicyKit1.Error.Cancelled", "cancelled");
            return;
        };
        let (user2, cookie2) = (user.clone(), cookie.clone());
        match gio::spawn_blocking(move || converse(&user2, &cookie2, &password)).await {
            Ok(Ok(true)) => {
                invocation.return_value(None);
                return;
            }
            Ok(Ok(false)) => error = Some("Wrong password, try again".into()),
            Ok(Err(e)) => error = Some(e),
            Err(_) => error = Some("The password check failed".into()),
        }
    }
    invocation.return_dbus_error("org.freedesktop.PolicyKit1.Error.Failed", "authentication failed");
}

/// Shows the password dialog; `None` when the user cancels.
async fn ask(app: &adw::Application, message: &str, user: &str, cookie: &str, error: Option<&str>) -> Option<String> {
    let dialog = adw::AlertDialog::new(Some("Root access"), Some(message));
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    entry.set_placeholder_text(Some(&format!("Password for {user}")));
    entry.set_activates_default(true);
    body.append(&entry);
    if let Some(error) = error {
        let label = gtk::Label::new(Some(error));
        label.add_css_class("error");
        body.append(&label);
    }
    dialog.set_extra_child(Some(&body));
    dialog.add_responses(&[("cancel", "Cancel"), ("auth", "Authenticate")]);
    dialog.set_response_appearance("auth", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("auth"));
    dialog.set_close_response("cancel");
    dialog.set_focus(Some(&entry));
    DIALOGS.with(|d| d.borrow_mut().insert(cookie.to_owned(), dialog.clone()));
    let parent = app.active_window();
    let response = dialog.choose_future(parent.as_ref()).await;
    let cancelled = DIALOGS.with(|d| d.borrow_mut().remove(cookie)).is_none();
    (response == "auth" && !cancelled).then(|| entry.text().to_string())
}

/// Runs polkit's setuid helper, which checks the password with PAM and
/// reports the result to polkitd itself.
fn converse(user: &str, cookie: &str, password: &str) -> Result<bool, String> {
    let helper = HELPERS
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .ok_or("polkit-agent-helper-1 is not installed")?;
    let mut child = Command::new(helper)
        .arg(user)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no helper stdin")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("no helper stdout")?);
    writeln!(stdin, "{cookie}").map_err(|e| e.to_string())?;
    let mut success = false;
    for line in stdout.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let answer = if line.starts_with("PAM_PROMPT_ECHO_OFF") {
            password
        } else if line.starts_with("PAM_PROMPT_ECHO_ON") {
            user
        } else if line == "SUCCESS" {
            success = true;
            break;
        } else if line == "FAILURE" {
            break;
        } else {
            continue;
        };
        if writeln!(stdin, "{answer}").is_err() {
            break;
        }
    }
    drop(stdin);
    let _ = child.wait();
    Ok(success)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn start_time_and_own_user_resolve() {
        assert!(start_time() > 0);
        let own = unsafe { libc::getuid() };
        assert!(user_name(own).is_some());
    }
    #[test]
    fn picks_own_uid_then_first_admin() {
        let own = unsafe { libc::getuid() };
        let list = |uids: &[u32]| {
            let items: Vec<String> = uids
                .iter()
                .map(|u| format!("('unix-user', {{'uid': <uint32 {u}>}})"))
                .collect();
            glib::Variant::parse(
                Some(glib::VariantTy::new("a(sa{sv})").unwrap()),
                &format!("[{}]", items.join(", ")),
            )
            .unwrap()
        };
        assert_eq!(pick_user(&list(&[0, own])).map(|u| u.0), Some(own));
        if own != 0 {
            assert_eq!(pick_user(&list(&[0])).map(|u| u.0), Some(0));
        }
    }
}
