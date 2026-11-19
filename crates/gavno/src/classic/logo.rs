//! The distribution logo on the Start button.
//!
//! `LOGO=` of os-release names an icon ("gentoo", "fedora-logo-icon", ...).
//! Resolving it walks the icon theme, which touches the disk, so a short
//! thread does it and sends the chosen icon name to the event loop once.
use crate::Gavno;
use smithay::reexports::calloop::channel::{self, Event};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, OnceLock,
};

/// The resolved icon name and whether it is symbolic (drawn white-tinted).
pub type Logo = (String, bool);
static TX: OnceLock<Mutex<channel::Sender<(u64, Option<Logo>)>>> = OnceLock::new();
/// Newer requests supersede the results of older ones.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The value of `LOGO=` in an os-release file, unquoted.
pub fn parse_logo(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|line| {
        let value = line.trim().strip_prefix("LOGO=")?.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        (!value.is_empty()).then(|| value.to_owned())
    })
}

/// Icon names to try, best first. A non-empty `configured` name is the only
/// candidate; empty means automatic.
pub fn candidates(configured: &str, logo: Option<&str>) -> Vec<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return vec![configured.to_owned()];
    }
    let mut names = Vec::new();
    if let Some(logo) = logo {
        names.push(logo.to_owned());
        if !logo.ends_with("-symbolic") {
            names.push(format!("{logo}-symbolic"));
        }
    }
    names.push("distributor-logo".into());
    names
}

fn read_os_release() -> Option<String> {
    ["/etc/os-release", "/usr/lib/os-release"].iter().find_map(|p| std::fs::read_to_string(p).ok())
}

/// First candidate the icon theme can resolve.
pub fn resolve(names: &[String], theme: &str) -> Option<Logo> {
    names
        .iter()
        .find(|n| crate::icons::exists(n, theme))
        .map(|n| (n.clone(), n.ends_with("-symbolic")))
}

/// Install the receiver once; call `spawn` for every (re)configuration.
pub(crate) fn init(gv: &mut Gavno) {
    if TX.get().is_some() {
        return;
    }
    let (tx, rx) = channel::channel();
    let result = gv.loop_handle.insert_source(rx, |event, _, gv| {
        if let Event::Msg((generation, logo)) = event {
            if generation == GENERATION.load(Ordering::Relaxed) && gv.classic.taskbar.logo != logo {
                gv.classic.taskbar.logo = logo;
                gv.classic.taskbar.invalidate();
                gv.queue_redraw_all();
            }
        }
    });
    match result {
        Ok(_) => {
            let _ = TX.set(Mutex::new(tx));
        }
        Err(error) => tracing::warn!(%error, "cannot watch the distribution logo"),
    }
}

/// Resolve the logo again, off the compositor thread.
pub(crate) fn spawn(gv: &mut Gavno) {
    init(gv);
    let Some(tx) = TX.get() else { return };
    let tx = tx.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let configured = gv.config.classic.start_icon.clone();
    let theme = gv.config.dock.icon_theme.clone();
    let spawned = std::thread::Builder::new().name("start-logo".into()).spawn(move || {
        let logo = read_os_release().and_then(|text| parse_logo(&text));
        let names = candidates(&configured, logo.as_deref());
        let _ = tx.send((generation, resolve(&names, &theme)));
    });
    if let Err(error) = spawned {
        tracing::debug!(%error, "distribution logo lookup unavailable");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_and_bare_logo_values() {
        assert_eq!(parse_logo("NAME=Gentoo\nLOGO=gentoo\nID=gentoo\n").as_deref(), Some("gentoo"));
        assert_eq!(parse_logo("LOGO=\"fedora-logo-icon\"").as_deref(), Some("fedora-logo-icon"));
        assert_eq!(parse_logo("  LOGO='arch'  ").as_deref(), Some("arch"));
    }
    #[test]
    fn missing_or_empty_logo_is_none() {
        assert_eq!(parse_logo("NAME=X\nID=x\n"), None);
        assert_eq!(parse_logo("LOGO=\n"), None);
        assert_eq!(parse_logo("LOGO=\"\"\n"), None);
        assert_eq!(parse_logo("ANSI_COLOR=1\nBUILD_LOGO=x\n"), None);
    }
    #[test]
    fn auto_candidates_follow_the_documented_order() {
        assert_eq!(candidates("", Some("gentoo")), ["gentoo", "gentoo-symbolic", "distributor-logo"]);
        assert_eq!(candidates("  ", None), ["distributor-logo"]);
        assert_eq!(candidates("", Some("x-symbolic")), ["x-symbolic", "distributor-logo"]);
    }
    #[test]
    fn configured_icon_replaces_the_search() {
        assert_eq!(candidates("my-logo", Some("gentoo")), ["my-logo"]);
    }
    #[test]
    fn nothing_found_keeps_the_builtin_glyph() {
        assert_eq!(resolve(&["gavno-no-such-icon-zzz".into()], "gavno-no-such-theme"), None);
    }
}
