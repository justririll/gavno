//! Minimal Linux-PAM ABI: four functions, no session or credential changes.
//!
//! pam-client 0.5 duplicates secret CString responses without wiping the Rust
//! allocation. This adapter copies directly from Secret into PAM-owned malloc
//! memory instead, and avoids bindgen/libclang in the compositor's build.
use crate::greeter::ipc::Secret;
use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use zeroize::Zeroize;

// Layouts/constants from Linux-PAM security/_pam_types.h and pam_appl.h.
const SUCCESS: c_int = 0;
const BUF_ERR: c_int = 5;
const CONV_ERR: c_int = 19;
const ECHO_OFF: c_int = 1;
const ECHO_ON: c_int = 2;
const ERROR_MSG: c_int = 3;
const TEXT_INFO: c_int = 4;
#[repr(C)]
struct Message {
    style: c_int,
    text: *const c_char,
}
#[repr(C)]
struct Response {
    text: *mut c_char,
    return_code: c_int,
}
#[repr(C)]
struct PamConversation {
    callback: unsafe extern "C" fn(
        c_int,
        *const *const Message,
        *mut *mut Response,
        *mut c_void,
    ) -> c_int,
    data: *mut c_void,
}
#[link(name = "pam")]
extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conversation: *const PamConversation,
        handle: *mut *mut c_void,
    ) -> c_int;
    fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_acct_mgmt(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
}
struct Conversation {
    user: String,
    password: Secret,
    answered: bool,
}

/// Only called synchronously by PAM with the live Conversation on the worker's stack.
/// No message text is read or logged, and no Rust allocation or panic crosses the ABI.
unsafe extern "C" fn converse(
    count: c_int,
    messages: *const *const Message,
    output: *mut *mut Response,
    data: *mut c_void,
) -> c_int {
    if output.is_null() {
        return CONV_ERR;
    }
    *output = std::ptr::null_mut();
    if !(1..=32).contains(&count) || messages.is_null() || data.is_null() {
        return CONV_ERR;
    }
    let count = count as usize;
    let responses = libc::calloc(count, std::mem::size_of::<Response>()).cast::<Response>();
    if responses.is_null() {
        return BUF_ERR;
    }
    let conversation = &mut *data.cast::<Conversation>();
    for index in 0..count {
        let message = *messages.add(index);
        if message.is_null() {
            free_responses(responses, count);
            return CONV_ERR;
        }
        let secret = (*message).style == ECHO_OFF;
        let bytes = match (*message).style {
            ECHO_ON => conversation.user.as_bytes(),
            ECHO_OFF if !conversation.answered => {
                conversation.answered = true;
                conversation.password.as_str().as_bytes()
            }
            ERROR_MSG | TEXT_INFO => continue,
            _ => {
                free_responses(responses, count);
                return CONV_ERR;
            }
        };
        if bytes.contains(&0) {
            free_responses(responses, count);
            return CONV_ERR;
        }
        let response = libc::malloc(bytes.len() + 1).cast::<u8>();
        if response.is_null() {
            free_responses(responses, count);
            return BUF_ERR;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), response, bytes.len());
        *response.add(bytes.len()) = 0;
        (*responses.add(index)).text = response.cast();
        if secret {
            conversation.password.clear();
        }
    }
    // Linux-PAM now owns the array and strings and wipes/frees its responses.
    *output = responses;
    SUCCESS
}
unsafe fn free_responses(responses: *mut Response, count: usize) {
    for index in 0..count {
        let text = (*responses.add(index)).text;
        if !text.is_null() {
            let length = libc::strlen(text);
            std::slice::from_raw_parts_mut(text.cast::<u8>(), length).zeroize();
            libc::free(text.cast());
        }
    }
    libc::free(responses.cast());
}

pub(super) fn authenticate(user: &str, password: Secret) -> bool {
    let service = service(std::fs::metadata("/etc/pam.d/gavno-lock"));
    let Ok(service) = CString::new(service) else {
        return false;
    };
    let Ok(username) = CString::new(user) else {
        return false;
    };
    let mut conversation = Conversation {
        user: user.into(),
        password,
        answered: false,
    };
    let callback = PamConversation {
        callback: converse,
        data: (&mut conversation as *mut Conversation).cast(),
    };
    let mut handle = std::ptr::null_mut();
    // The strings, callback and conversation outlive this entire transaction.
    unsafe {
        if pam_start(service.as_ptr(), username.as_ptr(), &callback, &mut handle) != SUCCESS
            || handle.is_null()
        {
            return false;
        }
        let mut status = pam_authenticate(handle, 1 /* PAM_DISALLOW_NULL_AUTHTOK */);
        if status == SUCCESS {
            status = pam_acct_mgmt(handle, 0);
        }
        conversation.password.clear();
        let ended = pam_end(handle, status);
        status == SUCCESS && ended == SUCCESS
    }
}
fn service(metadata: std::io::Result<std::fs::Metadata>) -> &'static str {
    // Only a missing file permits fallback, never permission or authentication failures.
    if metadata.is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        "login"
    } else {
        "gavno-lock"
    }
}

pub(super) fn current_user() -> Option<crate::greeter::User> {
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 16384];
    // Reentrant lookup uses the real uid, never USER/LOGNAME or a client supplied name.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    let entry = unsafe { entry.assume_init() };
    if entry.pw_name.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(entry.pw_name) }
        .to_str()
        .ok()?
        .to_owned();
    let display = if entry.pw_gecos.is_null() {
        name.clone()
    } else {
        let gecos = unsafe { CStr::from_ptr(entry.pw_gecos) }.to_string_lossy();
        let first = gecos.split(',').next().unwrap_or("").trim();
        if first.is_empty() {
            name.clone()
        } else {
            first.to_owned()
        }
    };
    Some(crate::greeter::User { name, display })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn conversation() -> Conversation {
        let mut password = Secret::new();
        for c in "secret🔒".chars() {
            password.push(c);
        }
        Conversation {
            user: "test".into(),
            password,
            answered: false,
        }
    }
    fn mock(conv: &mut Conversation, styles: &[i32]) -> (i32, *mut Response) {
        let messages: Vec<_> = styles
            .iter()
            .map(|style| Message {
                style: *style,
                text: c"Never log this".as_ptr(),
            })
            .collect();
        let pointers: Vec<_> = messages.iter().map(|m| m as *const Message).collect();
        let mut output = std::ptr::null_mut();
        let code = unsafe {
            converse(
                styles.len() as i32,
                pointers.as_ptr(),
                &mut output,
                (conv as *mut Conversation).cast(),
            )
        };
        (code, output)
    }
    #[test]
    fn mock_conversation_answers_once_and_wipes_password() {
        let mut conv = conversation();
        let (code, output) = mock(&mut conv, &[ECHO_ON, TEXT_INFO, ERROR_MSG, ECHO_OFF]);
        assert_eq!(code, SUCCESS);
        unsafe {
            assert_eq!(CStr::from_ptr((*output).text).to_bytes(), b"test");
            assert!((*output.add(1)).text.is_null());
            assert!((*output.add(2)).text.is_null());
            assert_eq!(
                CStr::from_ptr((*output.add(3)).text).to_str().unwrap(),
                "secret🔒"
            );
            free_responses(output, 4);
        }
        assert!(conv.password.is_empty());
        assert_eq!(
            mock(&mut conv, &[ECHO_OFF]),
            (CONV_ERR, std::ptr::null_mut())
        );
    }
    #[test]
    fn invalid_and_repeated_prompts_free_partial_results() {
        for styles in [&[ECHO_OFF, ECHO_OFF][..], &[ECHO_ON, 99][..], &[][..]] {
            assert_eq!(
                mock(&mut conversation(), styles),
                (CONV_ERR, std::ptr::null_mut())
            );
        }
        let mut conv = conversation();
        conv.password.push('\0');
        assert_eq!(
            mock(&mut conv, &[ECHO_OFF]),
            (CONV_ERR, std::ptr::null_mut())
        );
        let mut output = std::ptr::null_mut();
        assert_eq!(
            unsafe { converse(1, std::ptr::null(), &mut output, std::ptr::null_mut()) },
            CONV_ERR
        );
    }
    #[test]
    fn fallback_only_for_missing_service() {
        assert_eq!(service(Err(std::io::ErrorKind::NotFound.into())), "login");
        assert_eq!(
            service(Err(std::io::ErrorKind::PermissionDenied.into())),
            "gavno-lock"
        );
    }
}
