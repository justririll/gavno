use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    #[default]
    Grid,
    List,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub view: ViewMode,
    pub terminal: Option<String>,
    pub show_hidden: bool,
}

impl Config {
    pub fn path() -> PathBuf {
        glib::user_config_dir().join("gavno-files/config.toml")
    }

    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    // Configuration reads and writes run on the GIO worker pool.
    pub fn load() -> Result<Self, String> {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => Self::parse(&text).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _writer = WRITER.lock().map_err(|_| "Configuration writer failed")?;
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, text).map_err(|e| e.to_string())?;
        std::fs::rename(temporary, path).map_err(|e| e.to_string())
    }

    pub fn terminal_command(&self) -> String {
        std::env::var("TERMINAL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.terminal.clone())
            .unwrap_or_else(|| "foot".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_defaults_and_roundtrip() {
        let c = Config::parse("").expect("empty config");
        assert_eq!(c.view, ViewMode::Grid);
        let c = Config::parse("view = 'list'\nterminal = 'foot -a files'\nshow_hidden = true")
            .expect("valid config");
        assert_eq!(c.view, ViewMode::List);
        assert_eq!(c.terminal.as_deref(), Some("foot -a files"));
        assert!(
            Config::parse(&toml::to_string(&c).expect("serialize"))
                .expect("roundtrip")
                .show_hidden
        );
        assert!(Config::parse("view = 'unknown'").is_err());
        assert!(Config::parse("terminal = 42").is_err());
    }
}
