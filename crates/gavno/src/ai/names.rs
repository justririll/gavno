//! Workspace names. A free heuristic always runs; the model refines a name
//! only while it already runs for another reason and the window set has been
//! stable for 30 s. A name the user set is never replaced.
use crate::Gavno;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

const EDITORS: &[&str] = &[
    "code", "codium", "vscodium", "zed", "nvim", "neovim", "helix", "emacs", "kate", "gedit", "sublime",
    "jetbrains", "idea", "pycharm", "clion", "rustrover", "goland", "webstorm", "lapce",
];
const TERMINALS: &[&str] = &[
    "foot", "footclient", "kitty", "alacritty", "konsole", "wezterm", "xterm", "ghostty", "tilix", "terminal",
];
const CHAT: &[&str] = &[
    "telegram", "discord", "element", "signal", "slack", "whatsapp", "teams", "zoom", "viber", "skype",
];
/// How long a window set must stay unchanged before the model may rename it.
const STABLE: Duration = Duration::from_secs(30);

fn tokens(app_id: &str) -> Vec<String> {
    app_id
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

fn any_token(app_id: &str, set: &[&str]) -> bool {
    tokens(app_id).iter().any(|t| set.contains(&t.as_str()))
}

/// Last segment of a reverse-DNS id, capitalized: "org.mozilla.firefox" -> "Firefox".
fn fallback_name(app_id: &str) -> String {
    let last = app_id.trim_end_matches(".desktop").rsplit('.').next().unwrap_or(app_id);
    let mut chars = last.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// A short name for the windows (`app_id`s, back to front) on a workspace.
/// `display` maps an app id to its desktop-entry name.
pub fn heuristic(app_ids: &[String], mut display: impl FnMut(&str) -> Option<String>) -> Option<String> {
    if app_ids.is_empty() {
        return None;
    }
    let editor = app_ids.iter().any(|a| any_token(a, EDITORS));
    let terminal = app_ids.iter().any(|a| any_token(a, TERMINALS));
    if editor && (terminal || app_ids.len() == 1) {
        return Some("Code".into());
    }
    if app_ids.iter().all(|a| any_token(a, CHAT)) {
        return Some("Chat".into());
    }
    // Dominant app; the frontmost one wins a tie.
    let mut best: Option<(&String, usize)> = None;
    for app in app_ids {
        let count = app_ids.iter().filter(|a| *a == app).count();
        if best.is_none_or(|(_, c)| count >= c) {
            best = Some((app, count));
        }
    }
    let app = best?.0;
    let name = display(app).filter(|n| !n.is_empty()).unwrap_or_else(|| fallback_name(app));
    Some(name.chars().take(24).collect())
}

/// Order-independent signature of a window set, by app id only: titles change
/// constantly and must not restart the stability clock.
pub fn signature(app_ids: &[String]) -> u64 {
    let mut sorted: Vec<String> = app_ids.iter().map(|a| a.to_lowercase()).collect();
    sorted.sort();
    let mut hasher = DefaultHasher::new();
    sorted.hash(&mut hasher);
    hasher.finish()
}

impl Gavno {
    /// The active workspace's name on the active output, for the bar.
    pub(crate) fn active_workspace_name(&self) -> Option<String> {
        let output = self.active_output()?;
        let owner = self.workspaces.get(&output)?;
        owner.list.items.get(owner.list.active)?.name.clone()
    }

    /// Recomputes heuristic names after windows changed. Cheap: a few
    /// strings per workspace, no I/O besides the cached desktop entries.
    pub(crate) fn refresh_workspace_names(&mut self) {
        let enabled = self.config.ai.workspace_names;
        let mut updates = Vec::new();
        for (oi, owner) in self.workspaces.outputs.iter().enumerate() {
            for (wi, ws) in owner.list.items.iter().enumerate() {
                if ws.user_named {
                    continue;
                }
                let apps: Vec<String> = ws.windows.iter().map(|w| crate::overlay::window_labels(&w.window).1).collect();
                updates.push((oi, wi, apps));
            }
        }
        let mut seen = Vec::new();
        for (oi, wi, apps) in updates {
            let sig = signature(&apps);
            let (key, current_name, keep_ai) = {
                let owner = &self.workspaces.outputs[oi];
                let ws = &owner.list.items[wi];
                ((owner.output.name(), ws.id), ws.name.clone(), ws.name_sig == Some(sig))
            };
            seen.push(key.clone());
            let entry = self.ai.stable.entry(key).or_insert((sig, Instant::now()));
            if entry.0 != sig {
                *entry = (sig, Instant::now());
            }
            if keep_ai && enabled {
                continue;
            }
            let name = if enabled {
                let entries = &mut self.topbar.entries;
                heuristic(&apps, |app| entries.resolve(app).map(|e| e.name))
            } else {
                None
            };
            if name != current_name {
                let ws = &mut self.workspaces.outputs[oi].list.items[wi];
                ws.name = name;
                ws.name_sig = None;
            }
        }
        self.ai.stable.retain(|key, _| seen.contains(key));
        if enabled {
            self.ai_schedule_refine();
        }
    }

    /// Arms one timer for the next refinement, only while the model runs.
    fn ai_schedule_refine(&mut self) {
        if self.ai.refine_timer.is_some() || !self.ai.model_recent(self.config.ai.idle_stop_minutes) {
            return;
        }
        let now = Instant::now();
        let wait = self
            .ai_refinable()
            .into_iter()
            .map(|(_, _, _, since)| STABLE.saturating_sub(now.duration_since(since)))
            .min();
        let Some(wait) = wait else { return };
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(wait.max(Duration::from_millis(50)));
        self.ai.refine_timer = self
            .loop_handle
            .insert_source(timer, |_, _, gv| {
                let _stall = crate::perf::Stall::new("timer.ai_refine");
                gv.ai.refine_timer = None;
                gv.ai_refine_names();
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            })
            .ok();
    }

    /// Unnamed-by-user workspaces with windows whose name was not yet refined
    /// for the current window set: (output name, workspace id, signature, stable since).
    fn ai_refinable(&self) -> Vec<(String, u64, u64, Instant)> {
        let mut out = Vec::new();
        for owner in &self.workspaces.outputs {
            for ws in &owner.list.items {
                let key = (owner.output.name(), ws.id);
                if ws.user_named || ws.windows.is_empty() {
                    continue;
                }
                if let Some(&(sig, since)) = self.ai.stable.get(&key) {
                    if ws.name_sig != Some(sig) && !self.ai.requested.contains(&(key.clone(), sig)) {
                        out.push((key.0, key.1, sig, since));
                    }
                }
            }
        }
        out
    }

    fn ai_refine_names(&mut self) {
        if !self.config.ai.enabled || !self.config.ai.workspace_names || !self.ai.model_recent(self.config.ai.idle_stop_minutes) {
            return;
        }
        let now = Instant::now();
        for (output, id, sig, since) in self.ai_refinable() {
            if now.duration_since(since) < STABLE || self.ai.requested.len() > 64 {
                continue;
            }
            let windows: Vec<serde_json::Value> = self
                .workspaces
                .outputs
                .iter()
                .filter(|o| o.output.name() == output)
                .flat_map(|o| o.list.items.iter())
                .filter(|ws| ws.id == id)
                .flat_map(|ws| ws.windows.iter())
                .map(|w| {
                    let (title, app) = crate::overlay::window_labels(&w.window);
                    serde_json::json!({"app_id": app, "title": title.chars().take(60).collect::<String>()})
                })
                .take(12)
                .collect();
            self.ai.requested.insert(((output.clone(), id), sig));
            self.ai_send(super::Request::Name { output, ws: id, sig, windows: serde_json::Value::Array(windows).to_string() });
        }
        self.ai_schedule_refine();
    }

    /// Applies an AI name when the window set is still the one it was made for.
    pub(crate) fn ai_named(&mut self, output: &str, id: u64, sig: u64, name: String) {
        let apps_match = |gv: &Self| {
            gv.ai.stable.get(&(output.to_owned(), id)).is_some_and(|(s, _)| *s == sig)
        };
        if !apps_match(self) {
            return;
        }
        if let Some(ws) = self
            .workspaces
            .outputs
            .iter_mut()
            .filter(|o| o.output.name() == output)
            .flat_map(|o| o.list.items.iter_mut())
            .find(|ws| ws.id == id && !ws.user_named)
        {
            ws.name = Some(name);
            ws.name_sig = Some(sig);
            self.refresh_shell();
            self.queue_redraw_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(apps: &[&str]) -> Option<String> {
        let ids: Vec<String> = apps.iter().map(|s| s.to_string()).collect();
        heuristic(&ids, |_| None)
    }

    #[test]
    fn dominant_app_and_category_names() {
        assert_eq!(name(&[]), None);
        assert_eq!(name(&["org.mozilla.firefox"]).as_deref(), Some("Firefox"));
        assert_eq!(name(&["firefox", "foot", "firefox"]).as_deref(), Some("Firefox"));
        assert_eq!(name(&["code", "foot"]).as_deref(), Some("Code"));
        assert_eq!(name(&["dev.zed.Zed", "kitty", "firefox"]).as_deref(), Some("Code"));
        assert_eq!(name(&["org.telegram.desktop", "discord"]).as_deref(), Some("Chat"));
        assert_eq!(name(&["org.telegram.desktop", "firefox"]).as_deref(), Some("Firefox"));
        assert_eq!(name(&["foot"]).as_deref(), Some("Foot"));
    }

    #[test]
    fn desktop_entry_names_win_over_the_fallback() {
        let ids = vec!["foot".to_owned()];
        let r = heuristic(&ids, |app| (app == "foot").then(|| "Terminal".to_owned()));
        assert_eq!(r.as_deref(), Some("Terminal"));
        let long = heuristic(&ids, |_| Some("x".repeat(40))).unwrap();
        assert_eq!(long.chars().count(), 24);
    }

    #[test]
    fn signature_ignores_order_and_case() {
        let a = vec!["Foot".to_owned(), "firefox".to_owned()];
        let b = vec!["firefox".to_owned(), "foot".to_owned()];
        assert_eq!(signature(&a), signature(&b));
        assert_ne!(signature(&a), signature(&b[..1]));
    }
}
