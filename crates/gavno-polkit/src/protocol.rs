//! Session subjects, identities and the polkit PAM helper protocol.
use glib::prelude::*;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

pub const PATH: &str = "/org/gavno/PolkitAgent";
pub const NAME: &str = "org.freedesktop.PolicyKit1";
pub const AUTHORITY: &str = "/org/freedesktop/PolicyKit1/Authority";
pub const AUTHORITY_INTERFACE: &str = "org.freedesktop.PolicyKit1.Authority";
pub type Identity = (String, HashMap<String, glib::Variant>);

pub fn subject(session: &str) -> glib::Variant {
    (
        "unix-session",
        HashMap::from([("session-id", session.to_variant())]),
    )
        .to_variant()
}

pub fn registration_params(session: &str, locale: &str) -> glib::Variant {
    glib::Variant::tuple_from_iter([subject(session), locale.to_variant(), PATH.to_variant()])
}

/// The injected fallback keeps environment precedence testable without logind.
fn session_id_with(
    environment: Option<String>,
    fallback: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    match environment.filter(|s| !s.trim().is_empty()) {
        Some(session) => Ok(session),
        None => fallback(),
    }
}

pub fn session_id(connection: &gio::DBusConnection) -> Result<String, String> {
    session_id_with(std::env::var("XDG_SESSION_ID").ok(), || {
        let reply = connection
            .call_sync(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "GetSessionByPID",
                Some(&(std::process::id(),).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                -1,
                gio::Cancellable::NONE,
            )
            .map_err(|e| e.to_string())?;
        let (path,) = reply
            .get::<(glib::variant::ObjectPath,)>()
            .ok_or("Invalid GetSessionByPID reply")?;
        let reply = connection
            .call_sync(
                Some("org.freedesktop.login1"),
                path.as_str(),
                "org.freedesktop.DBus.Properties",
                "Get",
                Some(&("org.freedesktop.login1.Session", "Id").to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                -1,
                gio::Cancellable::NONE,
            )
            .map_err(|e| e.to_string())?;
        let (value,) = reply
            .get::<(glib::Variant,)>()
            .ok_or("Invalid session Id reply")?;
        value
            .get::<String>()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Missing session Id".into())
    })
}

pub fn allowed_uids(identities: &[Identity]) -> Vec<u32> {
    let mut uids = Vec::new();
    for (kind, properties) in identities {
        if kind == "unix-user" {
            if let Some(uid) = properties.get("uid").and_then(|v| v.get::<u32>()) {
                if !uids.contains(&uid) {
                    uids.push(uid);
                }
            }
        }
    }
    uids
}

pub fn preferred_user(uids: &[u32], own: u32) -> Option<usize> {
    uids.iter()
        .position(|uid| *uid == own)
        .or_else(|| (!uids.is_empty()).then_some(0))
}

pub fn user_name(uid: u32) -> Option<String> {
    let mut size = 4096;
    loop {
        let mut buffer = vec![0u8; size];
        // getpwuid_r initializes the structure and returns pointers into buffer.
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
        if status == libc::ERANGE && size < 1024 * 1024 {
            size *= 2;
            continue;
        }
        if status != 0 || result.is_null() || entry.pw_name.is_null() {
            return None;
        }
        return Some(
            unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }
                .to_string_lossy()
                .into_owned(),
        );
    }
}

/// Cancellation can interrupt a helper blocked in PAM or waiting for input.
#[derive(Default)]
pub struct Helper {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl Helper {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut child) = self.child.lock() {
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
            }
        }
    }

    pub fn authenticate(&self, user: &str, cookie: &str, password: &str) -> Result<bool, String> {
        let helper = [
            "/usr/lib/polkit-1/polkit-agent-helper-1",
            "/usr/libexec/polkit-agent-helper-1",
            "/usr/lib/polkit/polkit-agent-helper-1",
        ]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .ok_or("polkit-agent-helper-1 is not installed")?;
        self.converse(Path::new(helper), user, cookie, password)
    }

    fn converse(
        &self,
        helper: &Path,
        user: &str,
        cookie: &str,
        password: &str,
    ) -> Result<bool, String> {
        if [user, cookie, password]
            .iter()
            .any(|s| s.contains(['\n', '\r']))
        {
            return Err("Unsupported newline in helper input".into());
        }
        let (mut stdin, stdout) = {
            let mut slot = self.child.lock().map_err(|_| "Helper lock failed")?;
            if self.cancelled.load(Ordering::SeqCst) {
                return Ok(false);
            }
            let mut child = Command::new(helper)
                .arg(user)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| e.to_string())?;
            let stdin = child.stdin.take();
            let stdout = child.stdout.take();
            *slot = Some(child);
            (stdin, stdout)
        };
        // Always kill and reap, including on I/O errors and truncated output.
        let result = (|| {
            let stdin = stdin.as_mut().ok_or("No helper stdin")?;
            let stdout = BufReader::new(stdout.ok_or("No helper stdout")?);
            writeln!(stdin, "{cookie}").map_err(|e| e.to_string())?;
            for line in stdout.lines() {
                let line = line.map_err(|e| e.to_string())?;
                if self.cancelled.load(Ordering::SeqCst) {
                    return Ok(false);
                }
                let answer = if line.starts_with("PAM_PROMPT_ECHO_OFF ") {
                    password
                } else if line.starts_with("PAM_PROMPT_ECHO_ON ") {
                    user
                } else if line == "SUCCESS" {
                    return Ok(true);
                } else if line == "FAILURE" {
                    return Ok(false);
                } else {
                    continue;
                };
                writeln!(stdin, "{answer}").map_err(|e| e.to_string())?;
            }
            Ok(false)
        })();
        drop(stdin);
        let child = self.child.lock().ok().and_then(|mut slot| slot.take());
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, sync::atomic::AtomicUsize};

    #[test]
    fn session_subject_and_registration_text() {
        let expected =
            glib::Variant::parse(None, "('unix-session', {'session-id': <'c7'>})").unwrap();
        assert_eq!(subject("c7"), expected);
        let params = registration_params("session'\\id", "en_US.UTF-8");
        assert_eq!(params.type_().as_str(), "((sa{sv})ss)");
        let parsed = glib::Variant::parse(Some(params.type_()), &params.print(true)).unwrap();
        assert_eq!(params, parsed);
    }

    #[test]
    fn identity_filtering_and_default_selection() {
        let identities = vec![
            (
                "unix-group".into(),
                HashMap::from([("uid".into(), 42u32.to_variant())]),
            ),
            (
                "unix-user".into(),
                HashMap::from([("uid".into(), "invalid".to_variant())]),
            ),
            ("unix-user".into(), HashMap::new()),
            (
                "unix-user".into(),
                HashMap::from([("uid".into(), 0u32.to_variant())]),
            ),
            (
                "unix-user".into(),
                HashMap::from([("uid".into(), 1000u32.to_variant())]),
            ),
        ];
        assert_eq!(allowed_uids(&identities), vec![0, 1000]);
        assert_eq!(preferred_user(&allowed_uids(&identities), 1000), Some(1));
        assert_eq!(preferred_user(&[0, 1000], 2000), Some(0));
        assert_eq!(preferred_user(&[], 1000), None);
        assert!(user_name(unsafe { libc::getuid() }).is_some());
    }

    #[test]
    fn environment_session_id_precedes_logind() {
        // Exercise the actual environment in an isolated process, avoiding
        // environment mutation while the other tests run on parallel threads.
        if std::env::var_os("GAVNO_POLKIT_TEST_SESSION").is_some() {
            assert_eq!(
                session_id_with(std::env::var("XDG_SESSION_ID").ok(), || {
                    panic!("must not query logind")
                }),
                Ok("test-session".into())
            );
            return;
        }
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "protocol::tests::environment_session_id_precedes_logind",
            ])
            .env("GAVNO_POLKIT_TEST_SESSION", "1")
            .env("XDG_SESSION_ID", "test-session")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert_eq!(
            session_id_with(Some("c42".into()), || panic!("must not query logind")),
            Ok("c42".into())
        );
        for value in [None, Some(String::new()), Some("  ".into())] {
            assert_eq!(
                session_id_with(value, || Ok("fallback".into())),
                Ok("fallback".into())
            );
        }
        assert!(session_id_with(None, || Err("logind unavailable".into())).is_err());
    }

    struct FakeHelper(std::path::PathBuf);
    impl FakeHelper {
        fn new(script: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "gavno-polkit-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("helper");
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }
    impl Drop for FakeHelper {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    #[test]
    fn helper_password_success_failure_and_echo_on() {
        let fake = FakeHelper::new("#!/bin/sh\nIFS= read -r cookie\nprintf 'PAM_PROMPT_ECHO_ON User:\\n'\nIFS= read -r user\nprintf 'PAM_TEXT_INFO Checking credentials\\nPAM_PROMPT_ECHO_OFF Password:\\n'\nIFS= read -r password\nif [ \"$cookie\" = cookie ] && [ \"$user\" = \"$1\" ] && [ \"$password\" = 'secret with spaces' ]; then printf 'SUCCESS\\n'; else printf 'FAILURE\\n'; fi\n");
        assert!(Helper::default()
            .converse(&fake.0, "admin", "cookie", "secret with spaces")
            .unwrap());
        assert!(!Helper::default()
            .converse(&fake.0, "admin", "cookie", "wrong")
            .unwrap());
        assert!(!Helper::default()
            .converse(&fake.0, "admin", "bad-cookie", "secret with spaces")
            .unwrap());
    }

    #[test]
    fn helper_truncated_output_and_cancellation() {
        let fake = FakeHelper::new("#!/bin/sh\nIFS= read -r cookie\nprintf 'PAM_PROMPT_ECHO_OFF Password:\\n'\nIFS= read -r password\nexit 1\n");
        assert!(!Helper::default()
            .converse(&fake.0, "admin", "cookie", "secret")
            .unwrap());
        let helper = Helper::default();
        helper.cancel();
        assert!(!helper
            .converse(&fake.0, "admin", "cookie", "secret")
            .unwrap());
        assert!(Helper::default()
            .converse(&fake.0, "admin", "cookie\ninjected", "secret")
            .is_err());
    }

    #[test]
    fn cancelling_running_helper_reaps_it() {
        let fake = FakeHelper::new("#!/bin/sh\nIFS= read -r cookie\nprintf 'PAM_PROMPT_ECHO_OFF Password:\\n'\nIFS= read -r password\nIFS= read -r wait_forever\n");
        let helper = std::sync::Arc::new(Helper::default());
        let worker_helper = helper.clone();
        let path = fake.0.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = worker_helper.converse(&path, "admin", "cookie", "secret");
            sender.send(result).unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while helper.child.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "Helper did not start");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        helper.cancel();
        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("Cancelled helper did not exit");
        assert!(!matches!(result, Ok(true)));
        worker.join().unwrap();
        assert!(helper.child.lock().unwrap().is_none());
    }
}
