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

struct RegisteredAgent {
    seq: u64,
    conn: gio::DBusConnection,
    reg_id: gio::RegistrationId,
    _watch: gio::SignalSubscription,
}

enum AuthState {
    Dialog(adw::AlertDialog),
    Conversing,
    Cancelled,
}

thread_local! {
    static NEXT_AGENT_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
    static AGENT: RefCell<Option<RegisteredAgent>> = const { RefCell::new(None) };
    static REGISTER_WAITERS: RefCell<Option<Vec<futures_channel::oneshot::Sender<Result<(), String>>>>> =
        const { RefCell::new(None) };
    static AUTHS: RefCell<HashMap<String, AuthState>> = RefCell::new(HashMap::new());
}

fn unregister_agent(agent: RegisteredAgent) {
    let _ = agent.conn.unregister_object(agent.reg_id);
}

/// Registers the agent once per process without blocking the GTK main thread.
pub async fn ensure(app: &adw::Application) -> Result<(), String> {
    if AGENT.with(|a| a.borrow().is_some()) {
        return Ok(());
    }
    let rx = REGISTER_WAITERS.with(|cell| {
        let mut guard = cell.borrow_mut();
        if let Some(waiters) = guard.as_mut() {
            let (tx, rx) = futures_channel::oneshot::channel();
            waiters.push(tx);
            Some(rx)
        } else {
            *guard = Some(Vec::new());
            None
        }
    });
    if let Some(rx) = rx {
        return rx
            .await
            .unwrap_or_else(|_| Err("Polkit registration cancelled".to_owned()));
    }

    let result = register(app)
        .await
        .map_err(|e| format!("Cannot start the password prompt: {e}"));
    let outcome = match result {
        Ok(registered) => {
            AGENT.with(|a| {
                if a.borrow().is_none() {
                    a.replace(Some(registered));
                } else {
                    unregister_agent(registered);
                }
            });
            Ok(())
        }
        Err(e) => Err(e),
    };
    let waiters = REGISTER_WAITERS.with(|cell| cell.borrow_mut().take().unwrap_or_default());
    for tx in waiters {
        let _ = tx.send(outcome.clone());
    }
    outcome
}

async fn register(app: &adw::Application) -> Result<RegisteredAgent, glib::Error> {
    let connection = gio::bus_get_future(gio::BusType::System).await?;
    let seq = NEXT_AGENT_SEQ.with(|s| {
        let cur = s.get();
        s.set(cur + 1);
        cur
    });
    let invalidated = std::rc::Rc::new(std::cell::Cell::new(false));
    let inv_sig = invalidated.clone();
    let _watch = connection.subscribe_to_signal(
        Some("org.freedesktop.DBus"),
        Some("org.freedesktop.DBus"),
        Some("NameOwnerChanged"),
        Some("/org/freedesktop/DBus"),
        Some(NAME),
        gio::DBusSignalFlags::NONE,
        move |_| {
            inv_sig.set(true);
            AGENT.with(|a| {
                let matches = a.borrow().as_ref().is_some_and(|reg| reg.seq == seq);
                if matches {
                    if let Some(old) = a.borrow_mut().take() {
                        unregister_agent(old);
                    }
                }
            });
        },
    );
    let _ = connection
        .call_future(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "StartServiceByName",
            Some(&(NAME, 0u32).to_variant()),
            Some(glib::VariantTy::new("(u)").expect("variant ty")),
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await;
    invalidated.set(false);
    let owner: String = connection
        .call_future(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            Some(&(NAME,).to_variant()),
            Some(glib::VariantTy::new("(s)").expect("variant ty")),
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await?
        .child_value(0)
        .get()
        .unwrap_or_default();
    let node = gio::DBusNodeInfo::for_xml(XML)?;
    let interface = node
        .lookup_interface("org.freedesktop.PolicyKit1.AuthenticationAgent")
        .expect("interface in XML");
    let app = app.downgrade();
    let expected_owner = owner.clone();
    let id = connection
        .register_object(PATH, &interface)
        .method_call(move |_, sender, _, _, method, params, invocation| {
            if sender != Some(expected_owner.as_str()) {
                invocation
                    .return_dbus_error("org.freedesktop.DBus.Error.AccessDenied", "not polkitd");
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
                    let cookie: String = params.child_value(4).get().unwrap_or_default();
                    AUTHS.with(|d| d.borrow_mut().insert(cookie, AuthState::Conversing));
                    glib::spawn_future_local(begin(app, params, invocation));
                }
                "CancelAuthentication" => {
                    let cookie: String = params.child_value(0).get().unwrap_or_default();
                    AUTHS.with(|d| {
                        let mut map = d.borrow_mut();
                        if let Some(state) = map.get_mut(&cookie) {
                            if let AuthState::Dialog(dialog) =
                                std::mem::replace(state, AuthState::Cancelled)
                            {
                                dialog.force_close();
                            }
                        }
                    });
                    invocation.return_value(None);
                }
                _ => invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    "unknown method",
                ),
            }
        })
        .build()?;
    let agent = RegisteredAgent {
        seq,
        conn: connection.clone(),
        reg_id: id,
        _watch,
    };
    let locale = std::env::var("LANG").unwrap_or_else(|_| "C".into());
    let text = format!(
        "(('unix-process', {{'pid': <uint32 {}>, 'start-time': <uint64 {}>}}), '{}', '{}')",
        std::process::id(),
        start_time(),
        locale.replace(['\'', '\\'], ""),
        PATH
    );
    let params =
        glib::Variant::parse(Some(glib::VariantTy::new("((sa{sv})ss)").unwrap()), &text)?;
    if let Err(e) = connection
        .call_future(
            Some(NAME),
            "/org/freedesktop/PolicyKit1/Authority",
            "org.freedesktop.PolicyKit1.Authority",
            "RegisterAuthenticationAgent",
            Some(&params),
            None,
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await
    {
        unregister_agent(agent);
        return Err(e);
    }
    if invalidated.get() {
        unregister_agent(agent);
        return Err(glib::Error::new(
            gio::IOErrorEnum::Cancelled,
            "polkitd restarted during agent registration",
        ));
    }
    Ok(agent)
}

fn start_time() -> u64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|stat| {
            let rest = stat.rsplit_once(')')?.1;
            rest.split_whitespace().nth(19)?.parse().ok()
        })
        .unwrap_or(0)
}

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
    let uid = if uids.contains(&own) {
        own
    } else {
        *uids.first()?
    };
    user_name(uid).map(|name| (uid, name))
}

fn user_name(uid: u32) -> Option<String> {
    let mut buffer = vec![0u8; 4096];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

fn is_auth_cancelled(cookie: &str) -> bool {
    AUTHS.with(|d| matches!(d.borrow().get(cookie), Some(AuthState::Cancelled) | None))
}

async fn begin(
    app: adw::Application,
    params: glib::Variant,
    invocation: gio::DBusMethodInvocation,
) {
    let message: String = params.child_value(1).get().unwrap_or_default();
    let cookie: String = params.child_value(4).get().unwrap_or_default();
    let Some((_, user)) = pick_user(&params.child_value(5)) else {
        AUTHS.with(|d| d.borrow_mut().remove(&cookie));
        invocation.return_dbus_error(
            "org.freedesktop.PolicyKit1.Error.Failed",
            "no user to authenticate as",
        );
        return;
    };
    if is_auth_cancelled(&cookie) {
        AUTHS.with(|d| d.borrow_mut().remove(&cookie));
        invocation.return_dbus_error("org.freedesktop.PolicyKit1.Error.Cancelled", "cancelled");
        return;
    }
    let mut error = None::<String>;
    for _ in 0..3 {
        let Some(password) = ask(&app, &message, &user, &cookie, error.as_deref()).await else {
            AUTHS.with(|d| d.borrow_mut().remove(&cookie));
            invocation.return_dbus_error("org.freedesktop.PolicyKit1.Error.Cancelled", "cancelled");
            return;
        };
        let (user2, cookie2) = (user.clone(), cookie.clone());
        let res = gio::spawn_blocking(move || converse(&user2, &cookie2, &password)).await;
        if is_auth_cancelled(&cookie) {
            AUTHS.with(|d| d.borrow_mut().remove(&cookie));
            invocation.return_dbus_error("org.freedesktop.PolicyKit1.Error.Cancelled", "cancelled");
            return;
        }
        match res {
            Ok(Ok(true)) => {
                AUTHS.with(|d| d.borrow_mut().remove(&cookie));
                invocation.return_value(None);
                return;
            }
            Ok(Ok(false)) => error = Some("Wrong password, try again".into()),
            Ok(Err(e)) => {
                AUTHS.with(|d| d.borrow_mut().remove(&cookie));
                invocation.return_dbus_error("org.freedesktop.PolicyKit1.Error.Failed", &e);
                return;
            }
            Err(_) => {
                AUTHS.with(|d| d.borrow_mut().remove(&cookie));
                invocation.return_dbus_error(
                    "org.freedesktop.PolicyKit1.Error.Failed",
                    "the password check failed",
                );
                return;
            }
        }
    }
    AUTHS.with(|d| d.borrow_mut().remove(&cookie));
    invocation.return_dbus_error(
        "org.freedesktop.PolicyKit1.Error.Failed",
        "authentication failed",
    );
}

async fn ask(
    app: &adw::Application,
    message: &str,
    user: &str,
    cookie: &str,
    error: Option<&str>,
) -> Option<String> {
    if is_auth_cancelled(cookie) {
        return None;
    }
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
    AUTHS.with(|d| {
        if let Some(state) = d.borrow_mut().get_mut(cookie) {
            if !matches!(state, AuthState::Cancelled) {
                *state = AuthState::Dialog(dialog.clone());
            }
        }
    });
    if is_auth_cancelled(cookie) {
        return None;
    }
    let parent = app.active_window();
    let response = dialog.choose_future(parent.as_ref()).await;
    let cancelled = AUTHS.with(|d| {
        let mut map = d.borrow_mut();
        match map.get_mut(cookie) {
            Some(AuthState::Cancelled) | None => true,
            Some(state) => {
                *state = AuthState::Conversing;
                false
            }
        }
    });
    (response == "auth" && !cancelled).then(|| entry.text().to_string())
}

fn converse(user: &str, cookie: &str, password: &str) -> Result<bool, String> {
    if user.contains(['\n', '\r'])
        || cookie.contains(['\n', '\r'])
        || password.contains(['\n', '\r'])
    {
        return Err("Invalid authentication input".into());
    }
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
    let res = (|| -> Result<bool, String> {
        let mut stdin = child.stdin.take().ok_or("no helper stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().ok_or("no helper stdout")?);
        writeln!(stdin, "{cookie}").map_err(|e| e.to_string())?;
        let mut saw_result = false;
        let mut success = false;
        let mut raw_line = Vec::new();
        loop {
            raw_line.clear();
            let n = stdout
                .read_until(b'\n', &mut raw_line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            let line = String::from_utf8_lossy(&raw_line);
            let line = line.trim_end_matches(['\r', '\n']);
            let answer = if line.starts_with("PAM_PROMPT_ECHO_OFF") {
                password
            } else if line.starts_with("PAM_PROMPT_ECHO_ON") {
                user
            } else if line == "SUCCESS" {
                saw_result = true;
                success = true;
                break;
            } else if line == "FAILURE" {
                saw_result = true;
                break;
            } else {
                continue;
            };
            if writeln!(stdin, "{answer}").is_err() {
                break;
            }
        }
        drop(stdin);
        if !saw_result {
            return Err("Authentication helper exited unexpectedly".into());
        }
        Ok(success)
    })();
    if res.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    match (res, status) {
        (Ok(true), Ok(st)) if !st.success() => Err("Authentication helper failed".into()),
        (r, _) => r,
    }
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
