//! The action whitelist and the validation of everything the model returns.
//!
//! The model is never trusted: each action must be one of the variants below,
//! carry no extra fields, and refer only to ids and values that exist in the
//! context GAVNO sent with the request.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MAX_ACTIONS: usize = 3;
pub const MAX_NAME_CHARS: usize = 24;
pub const MAX_QUERY_CHARS: usize = 120;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Launch { desktop_id: String },
    Focus { window: u64 },
    MoveToWorkspace { window: u64, workspace: u32 },
    SwitchWorkspace { workspace: u32 },
    NameWorkspace { workspace: u32, name: String },
    Music {
        command: MusicCommand,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
    OpenFolder { path: String },
    Set { key: SetKey, value: SetValue },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicCommand {
    Play,
    Pause,
    Next,
    Previous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetKey {
    Brightness,
    NightLight,
    Dnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SetValue {
    Bool(bool),
    /// Brightness in percent.
    Number(f64),
}

/// What GAVNO tells the model about the desktop; also the validation universe.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Context {
    pub windows: Vec<WindowInfo>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub apps: Vec<AppInfo>,
    pub music: Option<MusicInfo>,
    pub home: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct WindowInfo {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    /// 1-based, like the workspace arguments.
    pub workspace: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkspaceInfo {
    pub index: u32,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct MusicInfo {
    pub state: String,
    pub title: String,
}

/// Actions that passed validation, with a one-line English description each.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Interpretation {
    pub actions: Vec<Action>,
    pub summary: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    actions: Vec<serde_json::Value>,
}

/// Strips control characters and trims; used for every model-provided text.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_owned()
}

fn expand_home(path: &str, home: &str) -> PathBuf {
    match path.strip_prefix("~") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(format!("{home}{rest}"))
        }
        _ => PathBuf::from(path),
    }
}

impl Action {
    /// Checks one action against the context, normalizing text and paths.
    pub fn validate(self, ctx: &Context) -> Result<Action, String> {
        let workspace_ok = |n: u32| {
            if n >= 1 && (n as usize) <= ctx.workspaces.len().max(1) && n <= 10 {
                Ok(n)
            } else {
                Err(format!("workspace {n} does not exist"))
            }
        };
        match self {
            Action::Launch { desktop_id } => {
                let id = clean(&desktop_id);
                if ctx.apps.iter().any(|a| a.id == id) {
                    Ok(Action::Launch { desktop_id: id })
                } else {
                    Err(format!("unknown application {id:?}"))
                }
            }
            Action::Focus { window } => {
                if ctx.windows.iter().any(|w| w.id == window) {
                    Ok(Action::Focus { window })
                } else {
                    Err(format!("unknown window {window}"))
                }
            }
            Action::MoveToWorkspace { window, workspace } => {
                if !ctx.windows.iter().any(|w| w.id == window) {
                    return Err(format!("unknown window {window}"));
                }
                Ok(Action::MoveToWorkspace { window, workspace: workspace_ok(workspace)? })
            }
            Action::SwitchWorkspace { workspace } => {
                Ok(Action::SwitchWorkspace { workspace: workspace_ok(workspace)? })
            }
            Action::NameWorkspace { workspace, name } => {
                let name = clean(&name);
                if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
                    return Err("workspace name must be 1-24 characters".into());
                }
                Ok(Action::NameWorkspace { workspace: workspace_ok(workspace)?, name })
            }
            Action::Music { command, query } => {
                let query = query.map(|q| clean(&q)).filter(|q| !q.is_empty());
                match (command, &query) {
                    (MusicCommand::Play, Some(q)) if q.chars().count() <= MAX_QUERY_CHARS => {
                        Ok(Action::Music { command, query })
                    }
                    (MusicCommand::Play, Some(_)) => Err("music query is too long".into()),
                    (MusicCommand::Play, None) => Ok(Action::Music { command, query: None }),
                    (_, None) => Ok(Action::Music { command, query: None }),
                    (_, Some(_)) => Err("only play takes a query".into()),
                }
            }
            Action::OpenFolder { path } => {
                let home = std::fs::canonicalize(&ctx.home)
                    .map_err(|_| "home directory is unknown".to_string())?;
                let full = std::fs::canonicalize(expand_home(&clean(&path), &ctx.home))
                    .map_err(|_| format!("folder {path:?} does not exist"))?;
                if full.starts_with(&home) && full.is_dir() {
                    Ok(Action::OpenFolder { path: full.to_string_lossy().into_owned() })
                } else {
                    Err("folder is outside the home directory".into())
                }
            }
            Action::Set { key, value } => match (key, value) {
                (SetKey::Brightness, SetValue::Number(p)) if p.is_finite() && (5.0..=100.0).contains(&p) => {
                    Ok(Action::Set { key, value })
                }
                (SetKey::Brightness, _) => Err("brightness must be a number from 5 to 100".into()),
                (SetKey::NightLight | SetKey::Dnd, SetValue::Bool(_)) => Ok(Action::Set { key, value }),
                (_, _) => Err("this setting takes on or off".into()),
            },
        }
    }

    /// One line for the launcher's preview row.
    pub fn describe(&self, ctx: &Context) -> String {
        let window = |id: u64| {
            ctx.windows
                .iter()
                .find(|w| w.id == id)
                .map(|w| {
                    let app = ctx.apps.iter().find(|a| a.id == w.app_id).map(|a| a.name.as_str());
                    app.filter(|n| !n.is_empty())
                        .unwrap_or(if w.title.is_empty() { &w.app_id } else { &w.title })
                        .to_owned()
                })
                .unwrap_or_default()
        };
        match self {
            Action::Launch { desktop_id } => {
                let name = ctx.apps.iter().find(|a| &a.id == desktop_id).map(|a| a.name.as_str());
                format!("Open {}", name.unwrap_or(desktop_id))
            }
            Action::Focus { window: id } => format!("Switch to {}", window(*id)),
            Action::MoveToWorkspace { window: id, workspace } => {
                format!("Move {} to workspace {workspace}", window(*id))
            }
            Action::SwitchWorkspace { workspace } => format!("Go to workspace {workspace}"),
            Action::NameWorkspace { workspace, name } => {
                format!("Name workspace {workspace} \"{name}\"")
            }
            Action::Music { command: MusicCommand::Play, query: Some(q) } => format!("Search music for \"{q}\""),
            Action::Music { command: MusicCommand::Play, query: None } => "Play music".into(),
            Action::Music { command: MusicCommand::Pause, .. } => "Pause music".into(),
            Action::Music { command: MusicCommand::Next, .. } => "Next track".into(),
            Action::Music { command: MusicCommand::Previous, .. } => "Previous track".into(),
            Action::OpenFolder { path } => format!("Open folder {path}"),
            Action::Set { key: SetKey::Brightness, value: SetValue::Number(p) } => {
                format!("Set brightness to {}%", p.round())
            }
            Action::Set { key, value: SetValue::Bool(on) } => {
                let state = if *on { "on" } else { "off" };
                match key {
                    SetKey::NightLight => format!("Turn night light {state}"),
                    SetKey::Dnd => format!("Turn Do Not Disturb {state}"),
                    SetKey::Brightness => format!("Set brightness {state}"),
                }
            }
            Action::Set { .. } => "Change a setting".into(),
        }
    }
}

/// Parses and validates the model's `{"actions":[...]}` text.
pub fn interpret(model_text: &str, ctx: &Context) -> Result<Interpretation, String> {
    let reply: Reply = serde_json::from_str(model_text.trim())
        .map_err(|e| format!("the model returned unusable output ({e})"))?;
    if reply.actions.is_empty() {
        return Err("I could not turn that into an action".into());
    }
    if reply.actions.len() > MAX_ACTIONS {
        return Err("the model proposed too many actions".into());
    }
    let mut actions = Vec::new();
    for value in reply.actions {
        let action: Action = serde_json::from_value(value)
            .map_err(|e| format!("the model proposed an action that is not allowed ({e})"))?;
        actions.push(action.validate(ctx)?);
    }
    let summary = actions.iter().map(|a| a.describe(ctx)).collect();
    Ok(Interpretation { actions, summary })
}

/// Validates a `{"name":"..."}` workspace name reply.
pub fn workspace_name(model_text: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Name {
        name: String,
    }
    let Name { name } = serde_json::from_str(model_text.trim())
        .map_err(|e| format!("the model returned unusable output ({e})"))?;
    let name = clean(&name);
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err("workspace name must be 1-24 characters".into());
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            windows: vec![
                WindowInfo { id: 1, app_id: "firefox".into(), title: "Docs".into(), workspace: 1 },
                WindowInfo { id: 2, app_id: "foot".into(), title: "~".into(), workspace: 2 },
            ],
            workspaces: (1..=3).map(|i| WorkspaceInfo { index: i, name: String::new() }).collect(),
            apps: vec![
                AppInfo { id: "firefox".into(), name: "Firefox".into() },
                AppInfo { id: "foot".into(), name: "Terminal".into() },
            ],
            music: None,
            home: std::env::temp_dir().to_string_lossy().into_owned(),
        }
    }

    fn run(text: &str) -> Result<Interpretation, String> {
        interpret(text, &ctx())
    }

    #[test]
    fn valid_actions_pass_with_a_summary() {
        let r = run(r#"{"actions":[{"action":"move_to_workspace","window":1,"workspace":2}]}"#).unwrap();
        assert_eq!(r.summary, ["Move Firefox to workspace 2"]);
        let r = run(r#"{"actions":[{"action":"launch","desktop_id":"foot"},{"action":"set","key":"brightness","value":40}]}"#).unwrap();
        assert_eq!(r.summary, ["Open Terminal", "Set brightness to 40%"]);
        let r = run(r#"{"actions":[{"action":"music","command":"play","query":"calm piano"}]}"#).unwrap();
        assert_eq!(r.summary, ["Search music for \"calm piano\""]);
    }

    #[test]
    fn unknown_ids_and_out_of_range_values_are_rejected() {
        for bad in [
            r#"{"actions":[{"action":"focus","window":99}]}"#,
            r#"{"actions":[{"action":"launch","desktop_id":"rm -rf"}]}"#,
            r#"{"actions":[{"action":"move_to_workspace","window":1,"workspace":4}]}"#,
            r#"{"actions":[{"action":"move_to_workspace","window":1,"workspace":0}]}"#,
            r#"{"actions":[{"action":"switch_workspace","workspace":11}]}"#,
            r#"{"actions":[{"action":"name_workspace","workspace":1,"name":""}]}"#,
            r#"{"actions":[{"action":"name_workspace","workspace":1,"name":"abcdefghijklmnopqrstuvwxyz"}]}"#,
            r#"{"actions":[{"action":"set","key":"brightness","value":0}]}"#,
            r#"{"actions":[{"action":"set","key":"brightness","value":true}]}"#,
            r#"{"actions":[{"action":"set","key":"dnd","value":50}]}"#,
            r#"{"actions":[{"action":"music","command":"next","query":"x"}]}"#,
        ] {
            assert!(run(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn unlisted_actions_and_extra_fields_are_rejected() {
        for bad in [
            r#"{"actions":[{"action":"shell","command":"id"}]}"#,
            r#"{"actions":[{"action":"close","window":1}]}"#,
            r#"{"actions":[{"action":"lock"}]}"#,
            r#"{"actions":[{"action":"focus","window":1,"extra":true}]}"#,
            r#"{"actions":[{"action":"set","key":"volume","value":10}]}"#,
            r#"{"actions":[{"window":1}]}"#,
            r#"{"actions":[],"note":"x"}"#,
            r#"{"actions":[]}"#,
            r#"{"actions":[{"action":"switch_workspace","workspace":1},{"action":"switch_workspace","workspace":1},{"action":"switch_workspace","workspace":1},{"action":"switch_workspace","workspace":1}]}"#,
            "not json",
            "",
        ] {
            assert!(run(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn one_bad_action_rejects_the_whole_answer() {
        let text = r#"{"actions":[{"action":"focus","window":1},{"action":"focus","window":7}]}"#;
        assert!(run(text).is_err());
    }

    #[test]
    fn open_folder_must_exist_under_home() {
        let home = std::env::temp_dir().join(format!("gavno-ai-home-{}", std::process::id()));
        std::fs::create_dir_all(home.join("Downloads")).unwrap();
        std::os::unix::fs::symlink("/etc", home.join("escape")).ok();
        let mut c = ctx();
        c.home = home.to_string_lossy().into_owned();
        let ok = |p: &str| interpret(&format!(r#"{{"actions":[{{"action":"open_folder","path":{p:?}}}]}}"#), &c);
        let real = std::fs::canonicalize(home.join("Downloads")).unwrap();
        let r = ok("~/Downloads").unwrap();
        assert_eq!(r.actions, [Action::OpenFolder { path: real.to_string_lossy().into_owned() }]);
        assert!(ok("~/Missing").is_err());
        assert!(ok("/etc").is_err());
        assert!(ok("~/escape").is_err());
        assert!(ok("~/../..").is_err());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn workspace_names_are_validated() {
        assert_eq!(workspace_name(r#"{"name":" Code "}"#).unwrap(), "Code");
        assert!(workspace_name(r#"{"name":""}"#).is_err());
        assert!(workspace_name(r#"{"name":"x","other":1}"#).is_err());
        assert!(workspace_name(&format!(r#"{{"name":"{}"}}"#, "x".repeat(25))).is_err());
    }
}
