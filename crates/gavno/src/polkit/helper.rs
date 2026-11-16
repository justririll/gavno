//! Identities, user names and the polkit PAM helper protocol.
//!
//! `polkit-agent-helper-1` is setuid: it reads the cookie, runs the PAM
//! conversation over stdin/stdout and reports the result to polkitd itself.
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};
use zbus::zvariant::OwnedValue;

pub type Identity = (String, HashMap<String, OwnedValue>);

/// Unix users polkit accepts for this request, in order, without duplicates.
pub fn allowed_uids(identities: &[Identity]) -> Vec<u32> {
    let mut uids = Vec::new();
    for (kind, properties) in identities {
        if kind != "unix-user" {
            continue;
        }
        if let Some(uid) = properties.get("uid").and_then(|v| u32::try_from(v).ok()) {
            if !uids.contains(&uid) {
                uids.push(uid);
            }
        }
    }
    uids
}

/// The session's own user when allowed, otherwise the first allowed user.
pub fn preferred_user(uids: &[u32], own: u32) -> Option<usize> {
    uids.iter()
        .position(|uid| *uid == own)
        .or_else(|| (!uids.is_empty()).then_some(0))
}

pub fn user_name(uid: u32) -> Option<String> {
    let mut size = 4096;
    loop {
        let mut buffer = vec![0u8; size];
        // SAFETY: getpwuid_r fills `entry` with pointers into `buffer`, which
        // outlives every use of them below.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let status = unsafe {
            libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr().cast(), buffer.len(), &mut result)
        };
        if status == libc::ERANGE && size < 1024 * 1024 {
            size *= 2;
            continue;
        }
        if status != 0 || result.is_null() || entry.pw_name.is_null() {
            return None;
        }
        return Some(unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }.to_string_lossy().into_owned());
    }
}

pub fn helper_path() -> Option<PathBuf> {
    [
        "/usr/lib/polkit-1/polkit-agent-helper-1",
        "/usr/libexec/polkit-agent-helper-1",
        "/usr/lib/polkit/polkit-agent-helper-1",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

/// One helper run. Cancellation interrupts a helper blocked in PAM or waiting
/// for input, and the child is always killed and reaped.
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

    /// Ok(true) when PAM accepted the password.
    pub fn converse(&self, helper: &Path, user: &str, cookie: &str, password: &str) -> Result<bool, String> {
        if [user, cookie, password].iter().any(|s| s.contains(['\n', '\r'])) {
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
        let result = (|| {
            let stdin = stdin.as_mut().ok_or("No helper stdin")?;
            let stdout = BufReader::new(stdout.ok_or("No helper stdout")?);
            writeln!(stdin, "{cookie}").map_err(|e| e.to_string())?;
            // Bounded: a PAM conversation is a handful of short lines.
            for line in stdout.lines().take(64) {
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
pub(crate) mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, sync::atomic::AtomicUsize};

    fn uid(value: u32) -> OwnedValue {
        OwnedValue::from(value)
    }

    #[test]
    fn identity_filtering_and_default_selection() {
        let identities: Vec<Identity> = vec![
            ("unix-group".into(), HashMap::from([("uid".into(), uid(42))])),
            ("unix-user".into(), HashMap::from([("uid".into(), OwnedValue::try_from(zbus::zvariant::Value::from("x")).unwrap())])),
            ("unix-user".into(), HashMap::new()),
            ("unix-user".into(), HashMap::from([("uid".into(), uid(0))])),
            ("unix-user".into(), HashMap::from([("uid".into(), uid(1000))])),
            ("unix-user".into(), HashMap::from([("uid".into(), uid(1000))])),
        ];
        assert_eq!(allowed_uids(&identities), vec![0, 1000]);
        assert_eq!(preferred_user(&allowed_uids(&identities), 1000), Some(1));
        assert_eq!(preferred_user(&[0, 1000], 2000), Some(0));
        assert_eq!(preferred_user(&[], 1000), None);
        assert!(user_name(unsafe { libc::getuid() }).is_some());
    }

    pub(crate) struct FakeHelper(pub PathBuf);
    impl FakeHelper {
        pub(crate) fn new(script: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "gavno-polkit-helper-{}-{}",
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

    pub(crate) const ACCEPTS_SECRET: &str = "#!/bin/sh\nIFS= read -r cookie\nprintf 'PAM_PROMPT_ECHO_ON User:\\n'\nIFS= read -r user\nprintf 'PAM_TEXT_INFO Checking\\nPAM_PROMPT_ECHO_OFF Password:\\n'\nIFS= read -r password\nif [ \"$user\" = \"$1\" ] && [ \"$password\" = 'secret with spaces' ]; then printf 'SUCCESS\\n'; else printf 'FAILURE\\n'; fi\n";

    #[test]
    fn helper_password_success_failure_and_echo_on() {
        let fake = FakeHelper::new(ACCEPTS_SECRET);
        assert!(Helper::default().converse(&fake.0, "admin", "cookie", "secret with spaces").unwrap());
        assert!(!Helper::default().converse(&fake.0, "admin", "cookie", "wrong").unwrap());
    }

    #[test]
    fn helper_truncated_output_cancellation_and_newlines() {
        let fake = FakeHelper::new("#!/bin/sh\nIFS= read -r cookie\nprintf 'PAM_PROMPT_ECHO_OFF Password:\\n'\nIFS= read -r password\nexit 1\n");
        assert!(!Helper::default().converse(&fake.0, "admin", "cookie", "secret").unwrap());
        let helper = Helper::default();
        helper.cancel();
        assert!(!helper.converse(&fake.0, "admin", "cookie", "secret").unwrap());
        assert!(Helper::default().converse(&fake.0, "admin", "cookie\ninjected", "secret").is_err());
    }

    #[test]
    fn cancelling_a_running_helper_reaps_it() {
        let fake = FakeHelper::new("#!/bin/sh\nIFS= read -r cookie\nprintf 'PAM_PROMPT_ECHO_OFF Password:\\n'\nIFS= read -r password\nIFS= read -r wait_forever\n");
        let helper = std::sync::Arc::new(Helper::default());
        let worker_helper = helper.clone();
        let path = fake.0.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender.send(worker_helper.converse(&path, "admin", "cookie", "secret")).unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while helper.child.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "helper did not start");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        helper.cancel();
        let result = receiver.recv_timeout(std::time::Duration::from_secs(5)).expect("cancelled helper did not exit");
        assert!(!matches!(result, Ok(true)));
        worker.join().unwrap();
        assert!(helper.child.lock().unwrap().is_none());
    }
}
