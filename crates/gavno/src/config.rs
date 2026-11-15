//! Runtime configuration.
//!
//! Values come from, in increasing priority: built-in defaults, a TOML file
//! (`$GAVNO_CONFIG` if set, else `$XDG_CONFIG_HOME/gavno/config.toml`, else
//! `$HOME/.config/gavno/config.toml`) and the `GAVNO_*` environment variables.
//! Live reload of the file is planned but not implemented yet; keep the field
//! names stable.

use std::{ffi::OsString, path::PathBuf};

use serde::Deserialize;
use smithay::input::keyboard::ModifiersState;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Modifier for compositor binds: Super on the TTY, Alt when nested
    /// (the host compositor keeps Super for itself).
    pub mod_key: ModKey,
    /// Terminal started by Mod+Return.
    pub terminal: String,
    /// Launcher started by Mod+D, if any.
    pub launcher: Option<String>,
    /// Output scale for built-in panels (eDP). External outputs use 1.0.
    pub internal_scale: f64,
    pub xkb_layout: String,
    pub xkb_variant: String,
    pub xkb_options: Option<String>,
    /// Gap between a new window and the work area edge, logical px.
    pub gap: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModKey {
    Super,
    Alt,
}

impl ModKey {
    pub fn is_held(self, mods: &ModifiersState) -> bool {
        match self {
            ModKey::Super => mods.logo,
            ModKey::Alt => mods.alt,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    mod_key: Option<ModKey>,
    terminal: Option<String>,
    launcher: Option<String>,
    gap: Option<i64>,
    output: Output,
    keyboard: Keyboard,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Output {
    internal_scale: Option<f64>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Keyboard {
    layout: Option<String>,
    variant: Option<String>,
    options: Option<String>,
    #[serde(flatten)]
    unknown: toml::Table,
}

impl Config {
    pub fn load(nested: bool) -> Self {
        let path = config_path(|name| std::env::var_os(name));
        let text = path.as_ref().and_then(|path| match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(error) => {
                if error.kind() == std::io::ErrorKind::NotFound {
                    tracing::debug!(path = %path.display(), "no config file");
                } else {
                    tracing::warn!(path = %path.display(), %error, "cannot read config file");
                }
                None
            }
        });
        let (config, warnings) = Self::build(nested, text.as_deref(), |name| std::env::var(name).ok());
        for warning in warnings {
            tracing::warn!(?path, "{warning}");
        }
        config
    }

    /// Pure merge: defaults < file < environment. Empty environment values are unset.
    fn build(nested: bool, text: Option<&str>, env: impl Fn(&str) -> Option<String>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut file: FileConfig = text
            .map(|text| toml::from_str::<toml::Table>(text).and_then(toml::Table::try_into))
            .transpose()
            .unwrap_or_else(|error| {
                warnings.push(format!("invalid config file: {error}"));
                None
            })
            .unwrap_or_default();
        // Capture unknown fields during deserialization, so this list cannot drift from the schema.
        let mut unknown = Vec::new();
        for (prefix, extra) in
            [("", &file.unknown), ("output.", &file.output.unknown), ("keyboard.", &file.keyboard.unknown)]
        {
            unknown.extend(extra.keys().map(|key| format!("{prefix}{key}")));
        }
        if !unknown.is_empty() {
            warnings.push(format!("unknown config keys ignored: {}", unknown.join(", ")));
        }
        if file.output.internal_scale.is_some_and(|scale| !scale.is_finite()) {
            warnings.push("invalid config file: internal_scale must be finite".into());
            file = FileConfig::default();
        }
        let var = |name| env(name).filter(|value| !value.is_empty());
        let default_mod = if nested { ModKey::Alt } else { ModKey::Super };
        let scale =
            var("GAVNO_SCALE").map(|s| s.parse().unwrap_or(1.75)).or(file.output.internal_scale).unwrap_or(1.75);
        // NaN has no ordered clamp result. Infinities from the environment clamp to the bounds.
        let scale_clamped = if f64::is_nan(scale) { 1.75 } else { scale.clamp(0.5, 4.0) };
        if scale != scale_clamped {
            warnings.push(format!("internal_scale {scale} clamped to {scale_clamped}"));
        }
        let gap = file.gap.unwrap_or(12);
        if gap != gap.clamp(0, 64) {
            warnings.push(format!("gap {gap} clamped to {}", gap.clamp(0, 64)));
        }
        (
            Self {
                mod_key: var("GAVNO_MOD")
                    .map(|value| match value.as_str() {
                        "alt" => ModKey::Alt,
                        "super" => ModKey::Super,
                        _ => default_mod,
                    })
                    .unwrap_or(file.mod_key.unwrap_or(default_mod)),
                terminal: var("GAVNO_TERMINAL")
                    .or(file.terminal.filter(|s| !s.trim().is_empty()))
                    .unwrap_or_else(|| "foot".into()),
                launcher: var("GAVNO_LAUNCHER").or(file.launcher.filter(|s| !s.is_empty())),
                internal_scale: scale_clamped,
                xkb_layout: var("GAVNO_XKB_LAYOUT").or(file.keyboard.layout).unwrap_or_else(|| "us,ru".into()),
                xkb_variant: var("GAVNO_XKB_VARIANT").or(file.keyboard.variant).unwrap_or_default(),
                xkb_options: Some(
                    var("GAVNO_XKB_OPTIONS").or(file.keyboard.options).unwrap_or_else(|| "grp:alt_shift_toggle".into()),
                ),
                gap: gap.clamp(0, 64) as i32,
            },
            warnings,
        )
    }
}

fn config_path(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let path = |name| env(name).filter(|s| !s.is_empty()).map(PathBuf::from);
    path("GAVNO_CONFIG").or_else(|| {
        // XDG base directories must be absolute; a relative value is treated as unset.
        path("XDG_CONFIG_HOME")
            .filter(|p| p.is_absolute())
            .or_else(|| path("HOME").map(|p| p.join(".config")))
            .map(|p| p.join("gavno/config.toml"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
mod_key = "alt"
terminal = "kitty"
launcher = "rofi"
gap = 20
[output]
internal_scale = 2.0
[keyboard]
layout = "de"
variant = "nodeadkeys"
options = "caps:escape"
"#;

    fn build(nested: bool, text: Option<&str>, vars: &[(&str, &str)]) -> (Config, Vec<String>) {
        Config::build(nested, text, |key| vars.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).into()))
    }

    #[test]
    fn defaults_and_example() {
        let expected = Config {
            mod_key: ModKey::Super,
            terminal: "foot".into(),
            launcher: None,
            internal_scale: 1.75,
            xkb_layout: "us,ru".into(),
            xkb_variant: "".into(),
            xkb_options: Some("grp:alt_shift_toggle".into()),
            gap: 12,
        };
        assert_eq!(build(false, None, &[]), (expected.clone(), vec![]));
        let example = include_str!("../../../dist/config.toml");
        for nested in [false, true] {
            assert_eq!(build(nested, Some(example), &[]), build(nested, None, &[]));
        }
        let settings = example
            .lines()
            .filter_map(|line| line.strip_prefix("# "))
            .filter(|line| (line.contains(" = ") || line.starts_with('[')) && !line.starts_with("launcher"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(build(false, Some(&settings), &[]), (expected, vec![]));
    }

    #[test]
    fn full_and_partial_files() {
        let expected = Config {
            mod_key: ModKey::Alt,
            terminal: "kitty".into(),
            launcher: Some("rofi".into()),
            internal_scale: 2.0,
            xkb_layout: "de".into(),
            xkb_variant: "nodeadkeys".into(),
            xkb_options: Some("caps:escape".into()),
            gap: 20,
        };
        assert_eq!(build(false, Some(FULL), &[]), (expected, vec![]));
        let mut partial = build(false, None, &[]).0;
        partial.terminal = "kitty".into();
        assert_eq!(build(false, Some("terminal = 'kitty'\n[output]\n[keyboard]"), &[]), (partial, vec![]));
    }

    #[test]
    fn environment_precedence_and_legacy_fallbacks() {
        let vars = [
            ("GAVNO_MOD", "super"),
            ("GAVNO_TERMINAL", " "),
            ("GAVNO_LAUNCHER", "fuzzel"),
            ("GAVNO_SCALE", "3"),
            ("GAVNO_XKB_LAYOUT", "fr"),
            ("GAVNO_XKB_VARIANT", "azerty"),
            ("GAVNO_XKB_OPTIONS", " "),
        ];
        let mut expected = build(false, None, &vars).0;
        expected.gap = 20;
        assert_eq!(build(false, Some(FULL), &vars), (expected, vec![]));
        assert_eq!(build(false, None, &vars).0.terminal, " ");
        assert_eq!(build(false, Some(FULL), &vars.map(|(k, _)| (k, ""))), build(false, Some(FULL), &[]));
        for nested in [false, true] {
            let c = build(nested, Some(FULL), &[("GAVNO_MOD", "invalid"), ("GAVNO_SCALE", "bad")]).0;
            assert_eq!(c.mod_key, if nested { ModKey::Alt } else { ModKey::Super });
            assert_eq!(c.internal_scale, 1.75);
        }
    }

    #[test]
    fn nested_modifier() {
        assert_eq!(build(true, None, &[]).0.mod_key, ModKey::Alt);
        assert_eq!(build(true, Some("mod_key = 'super'"), &[]).0.mod_key, ModKey::Super);
        assert_eq!(build(true, Some(FULL), &[("GAVNO_MOD", "super")]).0.mod_key, ModKey::Super);
    }

    #[test]
    fn invalid_files_are_atomic_but_environment_still_applies() {
        for bad in [
            "gap = [",
            "gap = 'big'",
            "mod_key = 'ctrl'",
            "keyboard = 1",
            "[output]\ninternal_scale = nan",
            "[output]\ninternal_scale = inf",
            "[output]\ninternal_scale = -inf",
        ] {
            let text = format!("terminal = 'kitty'\n{bad}");
            let (c, warnings) = build(false, Some(&text), &[("GAVNO_LAUNCHER", "fuzzel")]);
            assert_eq!(c, build(false, None, &[("GAVNO_LAUNCHER", "fuzzel")]).0, "{bad}");
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(warnings[0].contains("invalid config file"));
        }
    }

    #[test]
    fn unknown_keys_warn_once() {
        let text = "terminal = 'kitty'\nbogus = 1\n[output]\nwat = 2\n[keyboard]\nwrong = true\n[unknown_section]\nnope = true";
        let (c, warnings) = build(false, Some(text), &[]);
        assert_eq!(c.terminal, "kitty");
        assert_eq!(warnings, ["unknown config keys ignored: bogus, unknown_section, output.wat, keyboard.wrong"]);
    }

    #[test]
    fn clamping_including_large_gaps_and_environment_scales() {
        for (raw, expected) in [(i64::MIN, 0), (-1, 0), (0, 0), (64, 64), (65, 64), (i64::MAX, 64)] {
            let (c, warnings) = build(false, Some(&format!("gap = {raw}")), &[]);
            assert_eq!(c.gap, expected);
            assert_eq!(warnings.len(), usize::from(raw != i64::from(expected)));
        }
        for (raw, expected) in
            [("0.1", 0.5), ("9.0", 4.0), ("0.5", 0.5), ("4.0", 4.0), ("nan", 1.75), ("inf", 4.0), ("-inf", 0.5)]
        {
            let (c, warnings) = build(false, Some(FULL), &[("GAVNO_SCALE", raw)]);
            assert_eq!(c.internal_scale, expected);
            assert_eq!(warnings.is_empty(), raw == "0.5" || raw == "4.0");
        }
        for (raw, expected) in [("0.1", 0.5), ("9.0", 4.0)] {
            let (c, warnings) = build(false, Some(&format!("[output]\ninternal_scale = {raw}")), &[]);
            assert_eq!(c.internal_scale, expected);
            assert_eq!(warnings.len(), 1);
        }
    }

    #[test]
    fn empty_file_strings() {
        let (c, warnings) = build(false, Some("terminal = '  '\nlauncher = ''\n[keyboard]\noptions = ''"), &[]);
        assert_eq!(c.terminal, "foot");
        assert_eq!(c.launcher, None);
        assert_eq!(c.xkb_options.as_deref(), Some(""));
        assert!(warnings.is_empty());
    }

    #[test]
    fn paths() {
        let path =
            |vars: &[(&str, &str)]| config_path(|key| vars.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).into()));
        assert_eq!(path(&[("GAVNO_CONFIG", "custom"), ("XDG_CONFIG_HOME", "/x")]), Some("custom".into()));
        assert_eq!(path(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/u")]), Some("/x/gavno/config.toml".into()));
        for xdg in ["", "relative"] {
            assert_eq!(
                path(&[("GAVNO_CONFIG", ""), ("XDG_CONFIG_HOME", xdg), ("HOME", "/u")]),
                Some("/u/.config/gavno/config.toml".into())
            );
        }
        assert_eq!(path(&[("HOME", "/u")]), Some("/u/.config/gavno/config.toml".into()));
        assert_eq!(path(&[]), None);
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(b"/tmp/config-\xff.toml".to_vec());
        assert_eq!(config_path(|key| (key == "GAVNO_CONFIG").then(|| path.clone())), Some(PathBuf::from(path)));
    }
}
