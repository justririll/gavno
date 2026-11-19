//! The `[ai]` table of the GAVNO config file; the service reads nothing else.
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub enabled: bool,
    pub idle_stop_minutes: u64,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self { enabled: false, idle_stop_minutes: 10 }
    }
}

impl AiConfig {
    /// Parses the whole config text; everything outside `[ai]` is ignored.
    pub fn parse(text: &str) -> Self {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct File {
            ai: AiConfig,
        }
        let mut config = toml::from_str::<toml::Table>(text)
            .ok()
            .and_then(|table| table.try_into::<File>().ok())
            .unwrap_or_default()
            .ai;
        config.idle_stop_minutes = config.idle_stop_minutes.clamp(1, 1440);
        config
    }

    /// Reads `GAVNO_CONFIG` or `~/.config/gavno/config.toml`; a missing file is the default.
    pub fn load() -> Self {
        let path = std::env::var_os("GAVNO_CONFIG")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                let base = std::env::var_os("XDG_CONFIG_HOME")
                    .filter(|p| !p.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
                Some(base.join("gavno/config.toml"))
            });
        path.and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_clamp() {
        assert_eq!(AiConfig::parse(""), AiConfig { enabled: false, idle_stop_minutes: 10 });
        let config = AiConfig::parse("terminal = 'x'\n[ai]\nenabled = true\nidle_stop_minutes = 0\nbackend = 'cpu'");
        assert_eq!(config, AiConfig { enabled: true, idle_stop_minutes: 1 });
        assert_eq!(AiConfig::parse("[ai]\nidle_stop_minutes = 99999").idle_stop_minutes, 1440);
        assert_eq!(AiConfig::parse("not toml ["), AiConfig::default());
    }
}
