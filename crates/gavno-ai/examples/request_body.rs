//! Prints the exact chat request `Interpret` would send, for latency
//! measurements against a llama-server (`design/ai.md`, "Measurements").
//!
//!   cargo run -p gavno-ai --example request_body -- "open the downloads folder"
//!
//! With `CONTEXT=1` it prints the sample context JSON instead (for `busctl` calls).
use gavno_ai::{
    action::{AppInfo, Context, WindowInfo, WorkspaceInfo},
    prompt,
};

fn main() {
    let query = std::env::args().nth(1).unwrap_or_else(|| "open the downloads folder".into());
    let apps = [
        ("firefox", "Firefox"), ("foot", "Terminal"), ("org.telegram.desktop", "Telegram"), ("code", "Visual Studio Code"),
        ("org.gnome.Nautilus", "Files"), ("spotify", "Spotify"), ("steam", "Steam"), ("btop", "btop++"),
        ("org.gnome.Zenity", "Zenity"), ("org.kde.konsole", "Konsole"), ("libreoffice-writer", "LibreOffice Writer"),
        ("gimp", "GIMP"), ("vlc", "VLC media player"), ("org.gnome.Calculator", "Calculator"), ("mpv", "mpv Media Player"),
    ];
    let mut ctx = Context {
        windows: vec![
            WindowInfo { id: 1, app_id: "firefox".into(), title: "Arch Wiki - Firefox".into(), workspace: 1 },
            WindowInfo { id: 2, app_id: "foot".into(), title: "~/Projects/ds".into(), workspace: 1 },
            WindowInfo { id: 3, app_id: "org.telegram.desktop".into(), title: "Telegram".into(), workspace: 2 },
            WindowInfo { id: 4, app_id: "code".into(), title: "main.rs - ds".into(), workspace: 3 },
        ],
        workspaces: (1..=4).map(|index| WorkspaceInfo { index, name: String::new() }).collect(),
        home: "/home/user".into(),
        ..Default::default()
    };
    // Pad to a realistic launcher list of about 60 apps.
    for i in 0..60 {
        let (id, name) = apps.get(i).map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((format!("org.example.App{i}"), format!("Example App {i}")));
        ctx.apps.push(AppInfo { id, name });
    }
    if std::env::var_os("CONTEXT").is_some() {
        ctx.home = std::env::var("HOME").unwrap_or_default();
        println!("{}", serde_json::to_string(&ctx).unwrap());
        return;
    }
    let body = prompt::chat_body(prompt::SYSTEM, &prompt::user_message(&ctx, &query), &prompt::actions_schema(), 128);
    println!("{body}");
}
