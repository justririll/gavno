//! Fixed prompts and JSON schemas. The system prompt never changes, so
//! llama-server's prompt cache keeps it (and the app list after it) warm.
use crate::action::{Context, MAX_ACTIONS, MAX_NAME_CHARS, MAX_QUERY_CHARS};
use serde_json::{json, Value};

pub const SYSTEM: &str = r#"You turn a user's request for their Linux desktop into a JSON list of actions. Reply with JSON only. The request may be in any language, often Russian or English.

Actions (use only these, at most 3):
{"action":"launch","desktop_id":"<id from Apps>"}
{"action":"focus","window":<id from Windows>}
{"action":"move_to_workspace","window":<id from Windows>,"workspace":<1-10>}
{"action":"switch_workspace","workspace":<1-10>}
{"action":"name_workspace","workspace":<1-10>,"name":"<up to 24 characters>"}
{"action":"music","command":"play","query":"<what to play>"} (command may also be pause, next or previous, then without query)
{"action":"open_folder","path":"~/<folder under home>"}
{"action":"set","key":"brightness","value":<5-100>} (also key night_light or dnd with value true or false)

Rules: take ids only from Apps and Windows. Workspace numbers start at 1 and must exist. If the request fits no action, reply {"actions":[]}.

Examples:
"move firefox to the second workspace" -> {"actions":[{"action":"move_to_workspace","window":<firefox id>,"workspace":2}]}
"открой папку загрузки" -> {"actions":[{"action":"open_folder","path":"~/Downloads"}]}
"поставь яркость на 30" -> {"actions":[{"action":"set","key":"brightness","value":30}]}
"play something calm" -> {"actions":[{"action":"music","command":"play","query":"calm"}]}
"выключи уведомления" -> {"actions":[{"action":"set","key":"dnd","value":true}]}"#;

pub const NAME_SYSTEM: &str = r#"You name a desktop workspace from the windows on it. Reply with JSON only: {"name":"<1-2 words, up to 24 characters>"}. Name the activity (for example Code, Chat, Browsing, Music, Design), not a single window title. Use English."#;

fn variant(action: &str, props: Value, required: &[&str]) -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert("action".into(), json!({"const": action}));
    if let Value::Object(extra) = props {
        properties.extend(extra);
    }
    let mut required: Vec<&str> = required.to_vec();
    required.insert(0, "action");
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

/// `response_format` schema for `Interpret`: valid output has the right shape
/// even before the whitelist validation in `action::interpret`.
pub fn actions_schema() -> Value {
    let ws = json!({"type":"integer","minimum":1,"maximum":10});
    let variants = vec![
        variant("launch", json!({"desktop_id":{"type":"string","maxLength":100}}), &["desktop_id"]),
        variant("focus", json!({"window":{"type":"integer","minimum":0}}), &["window"]),
        variant(
            "move_to_workspace",
            json!({"window":{"type":"integer","minimum":0},"workspace":ws}),
            &["window", "workspace"],
        ),
        variant("switch_workspace", json!({"workspace":ws}), &["workspace"]),
        variant(
            "name_workspace",
            json!({"workspace":ws,"name":{"type":"string","minLength":1,"maxLength":MAX_NAME_CHARS}}),
            &["workspace", "name"],
        ),
        variant(
            "music",
            json!({"command":{"enum":["play","pause","next","previous"]},
                   "query":{"type":"string","maxLength":MAX_QUERY_CHARS}}),
            &["command"],
        ),
        variant("open_folder", json!({"path":{"type":"string","maxLength":200}}), &["path"]),
        variant(
            "set",
            json!({"key":{"enum":["brightness","night_light","dnd"]},
                   "value":{"anyOf":[{"type":"number"},{"type":"boolean"}]}}),
            &["key", "value"],
        ),
    ];
    json!({"type":"object","properties":{"actions":{"type":"array","maxItems":MAX_ACTIONS,
        "items":{"anyOf":variants}}},"required":["actions"],"additionalProperties":false})
}

pub fn name_schema() -> Value {
    json!({"type":"object","properties":{"name":{"type":"string","minLength":1,"maxLength":MAX_NAME_CHARS}},
           "required":["name"],"additionalProperties":false})
}

/// The user message. The stable app list comes first and the volatile parts last,
/// so the longest possible prefix stays in llama-server's prompt cache.
pub fn user_message(ctx: &Context, query: &str) -> String {
    let mut out = String::from("Apps (id = name):\n");
    for app in &ctx.apps {
        out.push_str(&format!("{} = {}\n", app.id, app.name));
    }
    out.push_str("Windows (id: app, title, workspace):\n");
    for w in &ctx.windows {
        out.push_str(&format!("{}: {}, {}, {}\n", w.id, w.app_id, w.title.replace('\n', " "), w.workspace));
    }
    let names: Vec<String> = ctx
        .workspaces
        .iter()
        .map(|w| if w.name.is_empty() { w.index.to_string() } else { format!("{} ({})", w.index, w.name) })
        .collect();
    out.push_str(&format!("Workspaces: {}\n", names.join(", ")));
    if let Some(m) = &ctx.music {
        out.push_str(&format!("Music: {}, {}\n", m.state, m.title));
    }
    out.push_str(&format!("Request: {}", query.replace('\n', " ")));
    out
}

pub fn name_message(windows: &[(String, String)]) -> String {
    let list: Vec<String> = windows.iter().take(12).map(|(app, title)| format!("{app}: {title}")).collect();
    format!("Windows:\n{}", list.join("\n"))
}

pub fn chat_body(system: &str, user: &str, schema: &Value, max_tokens: u32) -> Value {
    json!({
        "model": "gavno-ai",
        "messages": [{"role":"system","content":system},{"role":"user","content":user}],
        "temperature": 0,
        "max_tokens": max_tokens,
        "cache_prompt": true,
        "response_format": {"type":"json_schema","json_schema":{"name":"reply","strict":true,"schema":schema}},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::*;

    #[test]
    fn user_message_keeps_stable_parts_first() {
        let ctx = Context {
            apps: vec![AppInfo { id: "foot".into(), name: "Terminal".into() }],
            windows: vec![WindowInfo { id: 3, app_id: "foot".into(), title: "a\nb".into(), workspace: 1 }],
            workspaces: vec![WorkspaceInfo { index: 1, name: "Code".into() }],
            ..Default::default()
        };
        let text = user_message(&ctx, "open it");
        let apps = text.find("foot = Terminal").unwrap();
        let windows = text.find("3: foot, a b, 1").unwrap();
        assert!(apps < windows && windows < text.find("Request: open it").unwrap());
        assert!(text.contains("1 (Code)"));
    }

    #[test]
    fn schema_lists_every_whitelisted_action() {
        let schema = actions_schema().to_string();
        for name in ["launch", "focus", "move_to_workspace", "switch_workspace", "name_workspace", "music", "open_folder", "set"] {
            assert!(schema.contains(&format!("\"const\":\"{name}\"")), "{name}");
        }
    }
}
