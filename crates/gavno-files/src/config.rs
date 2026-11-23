use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    #[default]
    Grid,
    List,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SortField {
    #[default]
    Name,
    Size,
    Modified,
    Type,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub view: ViewMode,
    pub terminal: Option<String>,
    pub show_hidden: bool,
    pub sort_field: SortField,
    pub sort_descending: bool,
    pub sidebar_width: Option<i32>,
    pub window_width: Option<i32>,
    pub window_height: Option<i32>,
}

impl Config {
    pub fn path() -> PathBuf {
        glib::user_config_dir().join("gavno-files/config.toml")
    }

    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn load() -> Result<Self, String> {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => Self::parse(&text).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
        static NEXT_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let _writer = WRITER.lock().map_err(|_| "Configuration writer failed")?;
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        let seq = NEXT_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = path.with_extension(format!("toml.{}.{seq}.tmp", std::process::id()));
        if let Err(e) = std::fs::write(&temporary, text) {
            let _ = std::fs::remove_file(&temporary);
            return Err(e.to_string());
        }
        if let Err(e) = std::fs::rename(&temporary, &path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(e.to_string());
        }
        Ok(())
    }

    pub fn terminal_command(&self) -> String {
        let env_term = std::env::var("TERMINAL").ok();
        resolve_terminal_with(
            env_term.as_deref(),
            self.terminal.as_deref(),
            foot_server_available(),
        )
    }
}

pub fn resolve_terminal_with(
    env_terminal: Option<&str>,
    config_terminal: Option<&str>,
    foot_server_ready: bool,
) -> String {
    if let Some(t) = env_terminal.map(str::trim).filter(|s| !s.is_empty()) {
        return t.to_owned();
    }
    if let Some(t) = config_terminal.map(str::trim).filter(|s| !s.is_empty()) {
        return t.to_owned();
    }
    if foot_server_ready {
        "footclient".to_owned()
    } else {
        "foot".to_owned()
    }
}

fn foot_server_available() -> bool {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return false;
    };
    let runtime = Path::new(&runtime);
    if !command_exists("footclient") {
        return false;
    }
    if let Ok(display) = std::env::var("WAYLAND_DISPLAY") {
        return runtime.join(format!("foot-{display}.sock")).exists();
    }
    runtime.join("foot.sock").exists()
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
}

fn command_exists(bin: &str) -> bool {
    if bin.contains('/') {
        return is_executable_file(Path::new(bin));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable_file(&dir.join(bin)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_and_roundtrip() {
        let c = Config::parse("").expect("empty config");
        assert_eq!(c.view, ViewMode::Grid);
        assert_eq!(c.sort_field, SortField::Name);
        assert!(!c.sort_descending);

        let c = Config::parse(
            "view = 'list'\nterminal = 'foot -a files'\nshow_hidden = true\nsort_field = 'modified'\nsort_descending = true\nsidebar_width = 220",
        )
        .expect("valid config");
        assert_eq!(c.view, ViewMode::List);
        assert_eq!(c.terminal.as_deref(), Some("foot -a files"));
        assert_eq!(c.sort_field, SortField::Modified);
        assert!(c.sort_descending);
        assert_eq!(c.sidebar_width, Some(220));
        let round = Config::parse(&toml::to_string(&c).expect("serialize")).expect("roundtrip");
        assert!(round.show_hidden);
        assert_eq!(round.sort_field, SortField::Modified);
        assert!(round.sort_descending);
        assert!(Config::parse("view = 'unknown'").is_err());
        assert!(Config::parse("terminal = 42").is_err());
    }

    #[test]
    fn terminal_precedence() {
        assert_eq!(
            resolve_terminal_with(Some("kitty"), Some("foot"), true),
            "kitty"
        );
        assert_eq!(
            resolve_terminal_with(Some("  "), Some("alacritty"), true),
            "alacritty"
        );
        assert_eq!(resolve_terminal_with(None, None, true), "footclient");
        assert_eq!(resolve_terminal_with(None, None, false), "foot");
    }
}
