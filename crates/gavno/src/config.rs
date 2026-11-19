//! Runtime configuration.
//!
//! Values come from, in increasing priority: built-in defaults, a TOML file
//! (`$GAVNO_CONFIG` if set, else `$XDG_CONFIG_HOME/gavno/config.toml`, else
//! `$HOME/.config/gavno/config.toml`) and the `GAVNO_*` environment variables.
//! Startup tolerates invalid files; explicit reload validates before mutation.

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use serde::{Deserialize, Serialize};
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
    /// File manager started by Mod+E.
    pub file_manager: String,
    /// Session polkit agent started with the session; empty disables it.
    pub polkit_agent: String,
    /// Commands started with a TTY session (when their program is installed),
    /// by default resident servers that make apps open instantly.
    pub autostart: Vec<String>,
    /// Output scale for built-in panels (eDP). External outputs use 1.0.
    pub internal_scale: f64,
    pub output_mode: Option<crate::hdr::mode::ModeRequest>,
    pub output_hdr: bool,
    pub outputs: BTreeMap<String, OutputConfig>,
    pub sdr_white_nits: f32,
    pub repeat_delay: i32,
    pub repeat_rate: i32,
    /// Start in keyboard-only code mode.
    pub code_mode: bool,
    /// Show the code-mode key hint strip in the top bar.
    pub bar_hints: bool,
    pub xkb_layout: String,
    pub xkb_variant: String,
    pub xkb_options: Option<String>,
    /// Gap between a new window and the work area edge, logical px.
    pub gap: i32,
    pub theme: ThemeConfig,
    pub modules: ModulesConfig,
    pub classic: crate::classic::ClassicConfig,
    pub dock: DockConfig,
    pub bar: BarConfig,
    pub tiling: TilingConfig,
    pub lock: LockConfig,
    pub notifications: NotificationsConfig,
    pub media: MediaConfig,
    pub display: DisplayConfig,
    pub clipboard: ClipboardConfig,
    pub dropdown: DropdownConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct DisplayConfig {
    pub off_after_idle_secs: u32,
    pub brightness_control: crate::brightness::Control,
    pub direct_scanout: bool,
    /// Warm the HDR white point in the evening, following the theme's sun model.
    pub night_light: bool,
    /// Kelvin at full strength (1900-6500).
    pub night_light_temperature: u32,
    /// Startup-only: when false the session exports GTK_A11Y=none and NO_AT_BRIDGE=1.
    pub accessibility: bool,
    #[serde(flatten)]
    unknown: toml::Table,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self { off_after_idle_secs: 600, brightness_control: Default::default(), direct_scanout: true, night_light: true, night_light_temperature: crate::night_light::DEFAULT_K, accessibility: false, unknown: Default::default() }
    }
}

/// Connector overrides; absent values inherit the global output settings.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct OutputConfig {
    pub mode: Option<String>,
    pub scale: Option<f64>,
    pub hdr: Option<bool>,
    pub enabled: Option<bool>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct LockConfig {
    pub after_idle_secs: u32,
    #[serde(flatten)]
    unknown: toml::Table,
}
impl Default for LockConfig {
    fn default() -> Self { Self { after_idle_secs: 300, unknown: Default::default() } }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct DockConfig {
    pub enabled: bool,
    pub pinned: Vec<String>,
    pub magnify: bool,
    pub autohide: bool,
    pub icon_theme: String,
    #[serde(flatten)]
    unknown: toml::Table,
}
impl Default for DockConfig {
    fn default() -> Self { Self { enabled: true, pinned: vec!["foot".into(), "firefox".into()], magnify: true, autohide: false, icon_theme: "Adwaita".into(), unknown: Default::default() } }
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct BarConfig {
    pub enabled: bool,
    #[serde(flatten)]
    unknown: toml::Table,
}
impl Default for BarConfig {
    fn default() -> Self { Self { enabled: true, unknown: Default::default() } }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct TilingConfig {
    pub default: bool,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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

/// A foot server makes `footclient` windows open in a few milliseconds, and
/// all terminal windows share one process and glyph cache, which also saves
/// memory. Heavier resident helpers (`gavno-files --daemon`) are opt-in.
pub fn default_autostart() -> Vec<String> {
    vec!["foot --server".into()]
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    mod_key: Option<ModKey>,
    terminal: Option<String>,
    launcher: Option<String>,
    file_manager: Option<String>,
    polkit_agent: Option<String>,
    autostart: Option<Vec<String>>,
    gap: Option<i64>,
    output: Output,
    outputs: BTreeMap<String, OutputConfig>,
    keyboard: Keyboard,
    theme: ThemeConfig,
    modules: ModulesConfig,
    classic: crate::classic::ClassicConfig,
    dock: DockConfig,
    bar: BarConfig,
    tiling: TilingConfig,
    lock: LockConfig,
    notifications: NotificationsConfig,
    media: MediaConfig,
    display: DisplayConfig,
    clipboard: ClipboardConfig,
    dropdown: DropdownConfig,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Output {
    internal_scale: Option<f64>,
    mode: Option<String>,
    hdr: Option<bool>,
    sdr_white_nits: Option<f32>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Keyboard {
    repeat_delay: Option<i32>,
    repeat_rate: Option<i32>,
    layout: Option<String>,
    variant: Option<String>,
    options: Option<String>,
    code_mode: Option<bool>,
    bar_hints: Option<bool>,
    #[serde(flatten)]
    unknown: toml::Table,
}

impl Config {
    pub fn output_settings(&self, name: &str, internal: bool) -> (Option<crate::hdr::mode::ModeRequest>, f64, bool, bool) {
        let output = self.outputs.get(name);
        (output.and_then(|o| o.mode.as_ref()).and_then(|m| m.parse().ok()).or(self.output_mode),
         output.and_then(|o| o.scale).unwrap_or(if internal { self.internal_scale } else { 1.0 }),
         output.and_then(|o| o.hdr).unwrap_or(self.output_hdr),
         output.and_then(|o| o.enabled).unwrap_or(true))
    }

    pub fn load(nested: bool) -> Self {
        Self::load_checked(nested).unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot load configuration; using defaults and environment");
            Self::build(nested, None, |name| std::env::var(name).ok()).0
        })
    }

    /// Called by a one-shot IPC worker, never by the compositor event loop.
    pub(crate) fn load_checked(nested: bool) -> Result<Self, String> {
        let path = config_path(|name|std::env::var_os(name));
        let text = if let Some(path) = path {
            use std::io::Read;
            match std::fs::File::open(&path) {
                Ok(file) => {
                    let mut text = String::new();
                    file.take(1024 * 1024 + 1).read_to_string(&mut text).map_err(|e| format!("{}: {e}", path.display()))?;
                    if text.len() > 1024 * 1024 { return Err("config file exceeds 1 MiB".into()); }
                    text
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        } else { String::new() };
        Self::parse_checked(nested, &text, |name| std::env::var(name).ok())
    }

    pub(crate) fn parse_checked(nested: bool, text: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let file: FileConfig = toml::from_str(text).map_err(|error: toml::de::Error| {
            let offset = error.span().map_or(0, |span| span.start).min(text.len());
            let prefix = &text.as_bytes()[..offset];
            let line = prefix.iter().filter(|b| **b == b'\n').count() + 1;
            let column = prefix.iter().rposition(|b| *b == b'\n').map_or(offset + 1, |i| offset - i);
            format!("{error}\nline {line}, column {column}")
        })?;
        if file.output.internal_scale.is_some_and(|v| !v.is_finite()) || file.output.sdr_white_nits.is_some_and(|v| !v.is_finite()) {
            let key=if file.output.internal_scale.is_some_and(|v|!v.is_finite()) {"internal_scale"} else {"sdr_white_nits"};
            let line=text.lines().position(|line|line.split_once('=').is_some_and(|(name,_)|name.trim()==key)).map_or(1,|i|i+1);
            return Err(format!("output.{key} must be finite (line {line}, column 1)"));
        }
        file.theme.validate()?;
        let (config, warnings) = Self::build(nested, Some(text), env);
        for warning in warnings { tracing::warn!(%warning, "configuration warning"); }
        Ok(config)
    }

    /// Effective configuration in TOML path shape, used only on explicit
    /// schema/reload requests and by tests, never by the render/idle paths.
    pub(crate) fn values(&self) -> serde_json::Value {
        use serde_json::json;
        let mode = self.output_mode.map(|m| format!("{}x{}{}", m.width, m.height,
            m.refresh.map(|r| format!("@{}", f64::from(r)/1000.0)).unwrap_or_default()));
        json!({"theme":self.theme,"mod_key":self.mod_key, "terminal":self.terminal, "launcher":self.launcher,
            "file_manager":self.file_manager,"polkit_agent":self.polkit_agent,"autostart":self.autostart,"gap":self.gap,
            "output":{"internal_scale":self.internal_scale,"mode":mode,"hdr":self.output_hdr,"sdr_white_nits":self.sdr_white_nits},
            "keyboard":{"layout":self.xkb_layout,"variant":self.xkb_variant,"options":self.xkb_options,"repeat_delay":self.repeat_delay,"repeat_rate":self.repeat_rate,"code_mode":self.code_mode,"bar_hints":self.bar_hints},
            "display":self.display,"media":self.media,"clipboard":self.clipboard,"dropdown":self.dropdown,"modules":self.modules,"classic":self.classic,"dock":self.dock,"bar":self.bar,"tiling":self.tiling,"lock":self.lock,"notifications":self.notifications,"outputs":self.outputs})
    }

    /// Pure merge: defaults < file < environment. Empty environment values are unset.
    pub(crate) fn build(nested: bool, text: Option<&str>, env: impl Fn(&str) -> Option<String>) -> (Self, Vec<String>) {
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
            [("display.", &file.display.unknown), ("", &file.unknown), ("output.", &file.output.unknown), ("keyboard.", &file.keyboard.unknown), ("classic.", &file.classic.unknown), ("dock.", &file.dock.unknown), ("bar.", &file.bar.unknown), ("tiling.", &file.tiling.unknown), ("media.", &file.media.unknown), ("clipboard.", &file.clipboard.unknown), ("dropdown.", &file.dropdown.unknown), ("notifications.", &file.notifications.unknown), ("lock.", &file.lock.unknown), ("modules.", &file.modules.unknown)]
        {
            unknown.extend(extra.keys().map(|key| format!("{prefix}{key}")));
        }
        unknown.extend(file.theme.unknown.keys().map(|key| format!("theme.{key}")));
        for (name, output) in &file.outputs {
            unknown.extend(output.unknown.keys().map(|key| format!("outputs.{name}.{key}")));
        }
        for (name, output) in &mut file.outputs {
            if output.scale.is_some_and(|s| !s.is_finite() || !(0.5..=3.0).contains(&s)) {
                warnings.push(format!("invalid outputs.{name}.scale; using global fallback"));
                output.scale = None;
            }
            if output.mode.as_ref().is_some_and(|m| m.parse::<crate::hdr::mode::ModeRequest>().is_err()) {
                warnings.push(format!("invalid outputs.{name}.mode; using global fallback"));
                output.mode = None;
            }
        }
        if !unknown.is_empty() {
            warnings.push(format!("unknown config keys ignored: {}", unknown.join(", ")));
        }
        let max_visible = file.notifications.max_visible.clamp(1, 3);
        if max_visible != file.notifications.max_visible {
            warnings.push(format!("notifications.max_visible {} clamped to {max_visible}", file.notifications.max_visible));
        }
        file.notifications.max_visible = max_visible;
        let temperature = file.display.night_light_temperature.clamp(crate::night_light::MIN_K, crate::night_light::NEUTRAL_K);
        if temperature != file.display.night_light_temperature {
            warnings.push(format!("display.night_light_temperature {} clamped to {temperature}", file.display.night_light_temperature));
        }
        file.display.night_light_temperature = temperature;
        if file.output.internal_scale.is_some_and(|scale| !scale.is_finite())
            || file.output.sdr_white_nits.is_some_and(|nits| !nits.is_finite()) {
            warnings.push("invalid config file: output scale and SDR white must be finite".into());
            file = FileConfig::default();
        }
        if let Err(error) = file.theme.validate() {
            warnings.push(error);
            file.theme = ThemeConfig::default();
        }
        file.theme.latitude = file.theme.latitude.or_else(|| coordinate(env("GAVNO_LAT"), 90.0));
        file.theme.longitude = file.theme.longitude.or_else(|| coordinate(env("GAVNO_LON"), 180.0));
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
        let output_mode = var("GAVNO_OUTPUT_MODE").or(file.output.mode).and_then(|text| match text.parse() {
            Ok(mode) => Some(mode),
            Err(error) => { warnings.push(format!("invalid output mode {text:?}: {error}; using preferred mode")); None }
        });
        let output_hdr = var("GAVNO_HDR").and_then(|s| match s.as_str() {
            "true" | "1" => Some(true), "false" | "0" => Some(false),
            _ => { warnings.push("invalid GAVNO_HDR; using file/default value".into()); None }
        }).or(file.output.hdr).unwrap_or(false);
        let white = var("GAVNO_SDR_WHITE_NITS").and_then(|s| s.parse::<f32>().ok())
            .or(file.output.sdr_white_nits).unwrap_or(203.0);
        let sdr_white_nits = if white.is_finite() { white.clamp(80.0, 400.0) } else { 203.0 };
        if white != sdr_white_nits { warnings.push(format!("SDR white {white} clamped to {sdr_white_nits} nits")); }
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
                file_manager: file
                    .file_manager
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "gavno-files".into()),
                polkit_agent: file
                    .polkit_agent
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_else(|| "builtin".into()),
                autostart: file.autostart.unwrap_or_else(default_autostart),
                internal_scale: scale_clamped,
                output_mode,
                output_hdr,
                outputs: file.outputs,
                sdr_white_nits,
                repeat_delay: file.keyboard.repeat_delay.unwrap_or(300).clamp(0, 5000),
                repeat_rate: file.keyboard.repeat_rate.unwrap_or(30).clamp(0, 100),
                code_mode: file.keyboard.code_mode.unwrap_or(false),
                bar_hints: file.keyboard.bar_hints.unwrap_or(true),
                xkb_layout: var("GAVNO_XKB_LAYOUT").or(file.keyboard.layout).unwrap_or_else(|| "us,ru".into()),
                xkb_variant: var("GAVNO_XKB_VARIANT").or(file.keyboard.variant).unwrap_or_default(),
                xkb_options: Some(
                    var("GAVNO_XKB_OPTIONS").or(file.keyboard.options).unwrap_or_else(|| "grp:alt_shift_toggle".into()),
                ),
                gap: gap.clamp(0, 64) as i32,
                theme: file.theme,
                modules: file.modules,
                classic: file.classic,
                dock: file.dock,
                bar: file.bar,
                tiling: file.tiling,
                lock: file.lock,
                notifications: file.notifications,
                media: file.media,
                display: file.display,
                clipboard: file.clipboard.clamped(),
                dropdown: file.dropdown.clamped(),
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
    fn direct_scanout_defaults_parses_and_validates() {
        assert!(Config::build(false, None, |_| None).0.display.direct_scanout);
        for value in [true, false] {
            let text = format!("[display]\ndirect_scanout = {value}");
            let config = Config::parse_checked(false, &text, |_| None).unwrap();
            assert_eq!(config.display.direct_scanout, value);
            assert_eq!(config.values()["display"]["direct_scanout"], value);
        }
        for value in ["'true'", "1", "[]"] {
            assert!(Config::parse_checked(false, &format!("[display]\ndirect_scanout = {value}"), |_| None).is_err());
        }
    }

    #[test]
    fn night_light_defaults_parses_and_clamps() {
        let defaults = Config::build(false, None, |_| None).0.display;
        assert!(defaults.night_light);
        assert_eq!(defaults.night_light_temperature, 3400);
        let config = Config::parse_checked(false, "[display]\nnight_light = false\nnight_light_temperature = 2700", |_| None).unwrap();
        assert!(!config.display.night_light);
        assert_eq!(config.values()["display"]["night_light_temperature"], 2700);
        for (text, expected) in [(100, 1900), (9000, 6500), (6500, 6500)] {
            let (config, warnings) = Config::build(false, Some(&format!("[display]\nnight_light_temperature = {text}")), |_| None);
            assert_eq!(config.display.night_light_temperature, expected);
            assert_eq!(!warnings.is_empty(), text != expected);
        }
        for bad in ["-1", "'warm'", "3400.5"] {
            assert!(Config::parse_checked(false, &format!("[display]\nnight_light_temperature = {bad}"), |_| None).is_err());
        }
    }

    #[test]
    fn accessibility_defaults_parses_and_validates() {
        assert!(!Config::build(false, None, |_| None).0.display.accessibility);
        for value in [true, false] {
            let text = format!("[display]\naccessibility = {value}");
            let config = Config::parse_checked(false, &text, |_| None).unwrap();
            assert_eq!(config.display.accessibility, value);
            assert_eq!(config.values()["display"]["accessibility"], value);
        }
        for value in ["'true'", "1", "[]"] {
            assert!(Config::parse_checked(false, &format!("[display]\naccessibility = {value}"), |_| None).is_err());
        }
    }

    #[test]
    fn keyboard_repeat_settings() {
        let config=build(false,Some("[keyboard]\nrepeat_delay=450\nrepeat_rate=25"),&[]).0;
        assert_eq!((config.repeat_delay,config.repeat_rate),(450,25));
        let config=build(false,Some("[keyboard]\nrepeat_delay=-1\nrepeat_rate=999"),&[]).0;
        assert_eq!((config.repeat_delay,config.repeat_rate),(0,100));
        assert_eq!(build(false,Some("[keyboard]\nrepeat_rate=0"),&[]).0.repeat_rate,0);
    }

    #[test]
    fn keyboard_code_mode_and_bar_hints() {
        let defaults = Config::build(false, None, |_| None).0;
        assert!(!defaults.code_mode);
        assert!(defaults.bar_hints);
        let config = Config::parse_checked(false, "[keyboard]\ncode_mode = true\nbar_hints = false", |_| None).unwrap();
        assert!(config.code_mode);
        assert!(!config.bar_hints);
        assert_eq!(config.values()["keyboard"]["code_mode"], true);
        assert_eq!(config.values()["keyboard"]["bar_hints"], false);
        for (key, bad) in [("code_mode", "'yes'"), ("bar_hints", "1")] {
            let text = format!("[keyboard]\n{key} = {bad}");
            assert!(Config::parse_checked(false, &text, |_| None).is_err(), "{key} = {bad}");
            let (_, warnings) = Config::build(false, Some(&text), |_| None);
            assert_eq!(warnings.len(), 1, "{key} = {bad}");
            assert!(warnings[0].starts_with("invalid config file"), "{warnings:?}");
        }
    }

    #[test]
    fn defaults_and_example() {
        let expected = Config {
            theme: Default::default(),
            modules: Default::default(),
            mod_key: ModKey::Super,
            terminal: "foot".into(),
            launcher: None,
            file_manager: "gavno-files".into(),
            polkit_agent: "builtin".into(),
            autostart: default_autostart(),
            internal_scale: 1.75,
            output_mode: None,
            output_hdr: false,
            outputs: Default::default(),
            sdr_white_nits: 203.0,
            repeat_delay: 300,
            repeat_rate: 30,
            code_mode: false,
            bar_hints: true,
            xkb_layout: "us,ru".into(),
            xkb_variant: "".into(),
            xkb_options: Some("grp:alt_shift_toggle".into()),
            gap: 12,
            classic: Default::default(),
            dock: Default::default(),
            bar: Default::default(),
            tiling: Default::default(),
            lock: Default::default(),
            notifications: Default::default(),
            media: Default::default(),
            display: Default::default(),
            clipboard: Default::default(),
            dropdown: Default::default(),
        };
        assert_eq!(build(false, None, &[]), (expected.clone(), vec![]));
        let example = include_str!("../../../dist/config.toml");
        for nested in [false, true] {
            assert_eq!(build(nested, Some(example), &[]), build(nested, None, &[]));
        }
        let settings = example
            .lines()
            .filter_map(|line| line.strip_prefix("# "))
            .filter(|line| (line.contains(" = ") || line.starts_with('[')) && !line.starts_with("launcher") && !line.starts_with("mode"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(build(false, Some(&settings), &[]), (expected, vec![]));
    }

    #[test]
    fn theme_validation_and_coordinate_precedence() {
        let env = |key: &str| match key { "GAVNO_LAT" => Some("-33.87".into()), "GAVNO_LON" => Some("151.21".into()), _ => None };
        let c = Config::parse_checked(true, "[theme]\nbackground='solid'\nsolid_color='#123AbC'\naccent='#c0ffee'\nlatitude=12.5", env).unwrap();
        assert_eq!(c.theme.latitude, Some(12.5));
        assert_eq!(c.theme.longitude, Some(151.21));
        assert_eq!(c.theme.solid()[0], 18.0 / 255.0);
        assert_eq!(Config::parse_checked(true, "", env).unwrap().theme.latitude, Some(-33.87));
        assert_eq!(c.theme.app_color_scheme, "dark");
        assert_eq!(Config::parse_checked(true, "[theme]\napp_color_scheme='light'", env).unwrap().theme.app_color_scheme, "light");
        for bad in ["background='image'", "solid_color='#abc'", "accent='blue'", "app_color_scheme='auto'", "latitude=90.1", "longitude=-180.1", "latitude=nan", "longitude=inf"] {
            assert!(Config::parse_checked(true, &format!("[theme]\n{bad}"), |_| None).is_err(), "{bad}");
        }
        for bad in ["nan", "inf", "999", "", "bad"] {
            let c = Config::parse_checked(true, "", |_| Some(bad.into())).unwrap();
            assert_eq!(c.theme.latitude, None);
            assert_eq!(c.theme.longitude, None);
        }
        assert!(parse_color("#abcdef").is_some());
        assert!(parse_color("#é1234").is_none());
    }

    #[test]
    fn full_and_partial_files() {
        let expected = Config {
            theme: Default::default(),
            modules: Default::default(),
            mod_key: ModKey::Alt,
            terminal: "kitty".into(),
            launcher: Some("rofi".into()),
            file_manager: "gavno-files".into(),
            polkit_agent: "builtin".into(),
            autostart: default_autostart(),
            internal_scale: 2.0,
            output_mode: None,
            output_hdr: false,
            outputs: Default::default(),
            sdr_white_nits: 203.0,
            repeat_delay: 300,
            repeat_rate: 30,
            code_mode: false,
            bar_hints: true,
            xkb_layout: "de".into(),
            xkb_variant: "nodeadkeys".into(),
            xkb_options: Some("caps:escape".into()),
            gap: 20,
            classic: Default::default(),
            dock: Default::default(),
            bar: Default::default(),
            tiling: Default::default(),
            lock: Default::default(),
            notifications: Default::default(),
            media: Default::default(),
            display: Default::default(),
            clipboard: Default::default(),
            dropdown: Default::default(),
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
    fn dock_and_bar_sections() {
        let (c,warnings) = build(false,Some("[dock]\npinned = ['foot', 'org.mozilla.firefox']\nmagnify = false\nautohide = true\nicon_theme = 'Papirus'\n[bar]\nenabled = false"),&[]);
        assert!(warnings.is_empty()); assert_eq!(c.dock.pinned,["foot","org.mozilla.firefox"]);
        assert!(!c.dock.magnify); assert!(c.dock.autohide); assert_eq!(c.dock.icon_theme,"Papirus"); assert!(!c.bar.enabled);
    }
    #[test]
    fn tiling_defaults_and_unknown_keys() {
        let (c,warnings) = build(false, Some("[tiling]\ndefault = true"), &[]);
        assert!(c.tiling.default); assert!(warnings.is_empty());
        let (c,warnings) = build(false, Some("[tiling]\nunknown = true"), &[]);
        assert!(!c.tiling.default); assert_eq!(warnings,["unknown config keys ignored: tiling.unknown"]);
    }

    #[test]
    fn hdr_output_settings() {
        let (c, warnings) = build(false, Some("[output]\nmode = '2880x1620@120'\nhdr = true\nsdr_white_nits = 250"), &[]);
        assert!(warnings.is_empty());
        assert_eq!(c.output_mode.unwrap().refresh, Some(120000));
        assert!(c.output_hdr);
        assert_eq!(c.sdr_white_nits, 250.0);
        let (c, warnings) = build(false, Some("[output]\nmode = 'bad'\nsdr_white_nits = 500"), &[]);
        assert_eq!(warnings.len(), 2);
        assert_eq!(c.output_mode, None);
        assert_eq!(c.sdr_white_nits, 400.0);
        let (c, warnings) = build(false, Some("[output]\nhdr = true"), &[("GAVNO_HDR", "false"), ("GAVNO_OUTPUT_MODE", "1920x1080@60"), ("GAVNO_SDR_WHITE_NITS", "1")]);
        assert!(!c.output_hdr);
        assert_eq!(c.output_mode.unwrap().width, 1920);
        assert_eq!(c.sdr_white_nits, 80.0);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn display_power_configuration() {
        assert_eq!(build(false, None, &[]).0.display.off_after_idle_secs, 600);
        for seconds in [0, 1, 600, u32::MAX] {
            let text = format!("[display]\noff_after_idle_secs = {seconds}\n[lock]\nafter_idle_secs = 0");
            let (config, warnings) = build(false, Some(&text), &[]);
            assert_eq!(config.display.off_after_idle_secs, seconds);
            assert_eq!(config.lock.after_idle_secs, 0);
            assert!(warnings.is_empty());
            assert!(Config::parse_checked(false, &text, |_| None).is_ok());
        }
        for raw in ["-1", "4294967296", "1.5", "true", "'never'"] {
            let text = format!("[display]\noff_after_idle_secs = {raw}");
            let (config, warnings) = build(false, Some(&text), &[]);
            assert_eq!(config.display.off_after_idle_secs, 600);
            assert_eq!(warnings.len(), 1);
            assert!(Config::parse_checked(false, &text, |_| None).is_err());
        }
    }

    #[test]
    fn lock_configuration() {
        assert_eq!(build(false, None, &[]).0.lock.after_idle_secs, 300);
        for seconds in [0, 1, 300, u32::MAX] {
            let (c, warnings) = build(false, Some(&format!("[lock]\nafter_idle_secs = {seconds}")), &[]);
            assert_eq!(c.lock.after_idle_secs, seconds); assert!(warnings.is_empty());
        }
        for raw in ["-1", "4294967296", "'never'"] {
            let (c, warnings) = build(false, Some(&format!("[lock]\nafter_idle_secs = {raw}")), &[]);
            assert_eq!(c.lock.after_idle_secs, 300); assert_eq!(warnings.len(), 1);
        }
        assert_eq!(build(false, Some("[lock]\nwrong = true"), &[]).1, ["unknown config keys ignored: lock.wrong"]);
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

/// Local-only theme settings. Explicit coordinates take precedence over the environment.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub background: String,
    pub solid_color: String,
    pub accent: String,
    /// Colour scheme advertised to apps through the Settings portal.
    pub app_color_scheme: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    #[serde(flatten)]
    unknown: toml::Table,
}
impl Default for ThemeConfig {
    fn default() -> Self {
        Self { background: "living".into(), solid_color: "#0e1016".into(), accent: "living".into(),
            app_color_scheme: "dark".into(), latitude: None, longitude: None, unknown: Default::default() }
    }
}
pub(crate) fn parse_color(text: &str) -> Option<[f32; 4]> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    let n = u32::from_str_radix(digits, 16).ok()?;
    Some([((n >> 16) & 255) as f32 / 255.0, ((n >> 8) & 255) as f32 / 255.0, (n & 255) as f32 / 255.0, 1.0])
}
fn coordinate(text: Option<String>, limit: f64) -> Option<f64> {
    text?.parse::<f64>().ok().filter(|v| v.is_finite() && (-limit..=limit).contains(v))
}
impl ThemeConfig {
    fn validate(&self) -> Result<(), String> {
        for (key, valid) in [
            ("background", matches!(self.background.as_str(), "living" | "solid")),
            ("solid_color", parse_color(&self.solid_color).is_some()),
            ("accent", self.accent == "living" || parse_color(&self.accent).is_some()),
            ("app_color_scheme", matches!(self.app_color_scheme.as_str(), "dark" | "light" | "none")),
            ("latitude", self.latitude.is_none_or(|v| v.is_finite() && (-90.0..=90.0).contains(&v))),
            ("longitude", self.longitude.is_none_or(|v| v.is_finite() && (-180.0..=180.0).contains(&v))),
        ] {
            if !valid { return Err(format!("invalid theme.{key}")); }
        }
        Ok(())
    }
    pub fn location(&self, utc_offset: i64) -> (f64, f64) {
        if let (Some(lat), Some(lon)) = (self.latitude, self.longitude) { return (lat, lon); }
        let (lat, lon) = crate::ui::theme::location(utc_offset);
        (self.latitude.unwrap_or(lat), self.longitude.unwrap_or(lon))
    }
    pub fn solid(&self) -> [f32; 4] {
        parse_color(&self.solid_color).unwrap_or(crate::render::CLEAR_COLOR)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ModulesConfig {
    pub theme: bool,
    pub classic: bool,
    pub polkit: bool,
    pub color_management: bool,
    pub dock: bool,
    pub bar: bool,
    pub overview: bool,
    pub tiling: bool,
    pub snap: bool,
    pub switcher: bool,
    pub gestures: bool,
    pub window_gestures: bool,
    pub resize_mode: bool,
    pub animations: bool,
    pub decorations: bool,
    pub ipc: bool,
    pub screencopy: bool,
    pub launcher: bool,
    pub notifications: bool,
    pub media: bool,
    #[serde(flatten)]
    unknown: toml::Table,
}
impl Default for ModulesConfig {
    fn default() -> Self {
        Self { theme: true, classic: true, polkit: true, dock: true, bar: true, overview: true, tiling: true, snap: true, switcher: true, gestures: true, window_gestures: true, resize_mode: true, animations: true, decorations: true, ipc: true, screencopy: true, launcher: true, notifications: true, media: true, color_management: true, unknown: Default::default() }
    }
}

#[cfg(test)]
mod module_tests {
    use super::*;
    use crate::module::{Feature, Features};
    #[test]
    fn modules_defaults_unknown_keys_and_legacy_switches() {
        let (config, warnings) = Config::build(false, Some("[modules]\ndock = false\nanimations = false\nunknown_feature = true\n[bar]\nenabled = false"), |_| None);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("modules.unknown_feature"));
        let features = Features::new(&config);
        assert!(!features.on(Feature::Dock));
        assert!(!features.on(Feature::Bar));
        assert!(!features.on(Feature::Animations));
        assert!(features.on(Feature::Tiling));
        let (config, warnings) = Config::build(false, Some("[modules]\ndock = true\n[dock]\nenabled = false"), |_| None);
        assert!(warnings.is_empty());
        assert!(!Features::new(&config).on(Feature::Dock));
        let config = Config::build(false, None, |_| None).0;
        assert!(Feature::ALL.into_iter().all(|f| Features::new(&config).on(f)));
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct NotificationsConfig {
    pub timeout_ms: u64,
    pub max_visible: usize,
    #[serde(flatten)]
    unknown: toml::Table,
}
impl Default for NotificationsConfig {
    fn default() -> Self { Self { timeout_ms: 5000, max_visible: 3, unknown: Default::default() } }
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    #[test]
    fn notification_defaults_clamps_and_module_toggle() {
        let (config, warnings) = Config::build(false, Some("[notifications]\ntimeout_ms = 1200\nmax_visible = 9\n[modules]\nnotifications = false"), |_| None);
        assert_eq!(config.notifications.timeout_ms, 1200);
        assert_eq!(config.notifications.max_visible, 3);
        assert_eq!(warnings.len(), 1);
        assert!(!crate::module::Features::new(&config).on(crate::module::Feature::Notifications));
        let (config, warnings) = Config::build(false, Some("[notifications]\nmax_visible = 0"), |_| None);
        assert_eq!(config.notifications.max_visible, 1);
        assert_eq!(config.notifications.timeout_ms, 5000);
        assert_eq!(warnings.len(), 1);
    }
}

#[cfg(test)]
mod output_settings_tests {
    use super::*;
    #[test]
    fn connector_overrides_and_global_fallbacks() {
        let (config, warnings) = Config::build(false, Some(r#"
[output]
internal_scale = 2.0
mode = "2880x1620@120"
hdr = true
[outputs."eDP-1"]
mode = "1920x1080@59.94"
scale = 1.75
hdr = false
enabled = false
[outputs."DP-2"]
scale = 1.25
"#), |_| None);
        assert!(warnings.is_empty(), "{warnings:?}");
        let (mode, scale, hdr, enabled) = config.output_settings("eDP-1", true);
        assert_eq!(mode.unwrap().refresh, Some(59940));
        assert_eq!((scale, hdr, enabled), (1.75, false, false));
        assert_eq!(config.output_settings("DP-2", false), (config.output_mode, 1.25, true, true));
        assert_eq!(config.output_settings("DP-3", false), (config.output_mode, 1.0, true, true));
        assert_eq!(config.output_settings("eDP-2", true), (config.output_mode, 2.0, true, true));
    }
    #[test]
    fn invalid_connector_values_use_fallbacks() {
        let (config, warnings) = Config::build(false, Some("[outputs.'eDP-1']\nmode = 'bad'\nscale = nan\nunknown = 1"), |_| None);
        assert_eq!(warnings.len(), 3);
        assert_eq!(config.output_settings("eDP-1", true), (None, 1.75, false, true));
        for scale in [0.49, 3.01, f64::INFINITY] {
            let (config, warnings) = Config::build(false, Some(&format!("[outputs.'DP-1']\nscale = {scale}")), |_| None);
            assert!(!warnings.is_empty());
            assert_eq!(config.output_settings("DP-1", false).1, 1.0);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct MediaConfig {
    pub music_panel: bool,
    #[serde(flatten)]
    unknown: toml::Table,
}
impl Default for MediaConfig {
    fn default() -> Self { Self { music_panel: true, unknown: Default::default() } }
}

/// Clipboard history, see `docs/screenshots.md`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ClipboardConfig {
    /// Record text and image selections in memory and enable Mod+V.
    pub history: bool,
    /// Entries kept, newest first.
    pub history_size: u32,
    #[serde(flatten)]
    unknown: toml::Table,
}
impl Default for ClipboardConfig {
    fn default() -> Self { Self { history: true, history_size: 50, unknown: Default::default() } }
}
impl ClipboardConfig {
    fn clamped(mut self) -> Self {
        self.history_size = self.history_size.clamp(1, 500);
        self
    }
}

#[cfg(test)]
mod clipboard_config_tests {
    use super::*;
    #[test]
    fn defaults_parse_clamp_and_report_unknown_keys() {
        let config = Config::build(false, None, |_| None).0;
        assert_eq!(config.clipboard, ClipboardConfig::default());
        assert!(config.clipboard.history);
        assert_eq!(config.clipboard.history_size, 50);
        let (config, warnings) = Config::build(false, Some("[clipboard]\nhistory = false\nhistory_size = 9999\nwrong = 1"), |_| None);
        assert_eq!(warnings, ["unknown config keys ignored: clipboard.wrong"]);
        assert!(!config.clipboard.history);
        assert_eq!(config.clipboard.history_size, 500);
        let (config, _) = Config::build(false, Some("[clipboard]\nhistory_size = 0"), |_| None);
        assert_eq!(config.clipboard.history_size, 1);
        assert_eq!(config.values()["clipboard"]["history"], true);
    }
}

/// The quake-style dropdown terminal, see `dropdown.rs`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct DropdownConfig {
    /// Shell command that starts the terminal; it must give its window the
    /// app id `gavno-dropdown`.
    pub command: String,
    /// Fraction of the work area height, 0.2-0.9.
    pub height: f64,
    /// Fraction of the work area width, 0.3-1.0, centred.
    pub width: f64,
    /// Slide the terminal away when another window takes keyboard focus.
    pub hide_on_focus_loss: bool,
    #[serde(flatten)]
    unknown: toml::Table,
}
impl Default for DropdownConfig {
    fn default() -> Self {
        Self { command: "foot --app-id gavno-dropdown".into(), height: 0.45, width: 1.0, hide_on_focus_loss: true, unknown: Default::default() }
    }
}
impl DropdownConfig {
    fn clamped(mut self) -> Self {
        let clamp = |value: f64, min: f64, max: f64, default: f64| if value.is_finite() { value.clamp(min, max) } else { default };
        self.height = clamp(self.height, 0.2, 0.9, 0.45);
        self.width = clamp(self.width, 0.3, 1.0, 1.0);
        if self.command.trim().is_empty() { self.command = Self::default().command; }
        self
    }
}

#[cfg(test)]
mod dropdown_config_tests {
    use super::*;
    #[test]
    fn defaults_parse_clamp_and_report_unknown_keys() {
        let config = Config::build(false, None, |_| None).0;
        assert_eq!(config.dropdown, DropdownConfig::default());
        assert_eq!(config.dropdown.command, "foot --app-id gavno-dropdown");
        assert_eq!((config.dropdown.height, config.dropdown.width), (0.45, 1.0));
        assert!(config.dropdown.hide_on_focus_loss);
        let text = "[dropdown]\ncommand = \"kitty --class gavno-dropdown\"\nheight = 0.6\nwidth = 0.8\nhide_on_focus_loss = false\nwrong = 1";
        let (config, warnings) = Config::build(false, Some(text), |_| None);
        assert_eq!(warnings, ["unknown config keys ignored: dropdown.wrong"]);
        assert_eq!(config.dropdown.command, "kitty --class gavno-dropdown");
        assert_eq!((config.dropdown.height, config.dropdown.width), (0.6, 0.8));
        assert!(!config.dropdown.hide_on_focus_loss);
        let (config, _) = Config::build(false, Some("[dropdown]\nheight = 5.0\nwidth = 0.0\ncommand = \"  \""), |_| None);
        assert_eq!((config.dropdown.height, config.dropdown.width), (0.9, 0.3));
        assert_eq!(config.dropdown.command, DropdownConfig::default().command);
        let (config, _) = Config::build(false, Some("[dropdown]\nheight = 0.01"), |_| None);
        assert_eq!(config.dropdown.height, 0.2);
        assert_eq!(config.values()["dropdown"]["height"], 0.2);
    }
}

#[cfg(test)]
mod music_config_tests {
    use super::*;
    #[test]
    fn panel_defaults_on_and_can_be_disabled_on_reload() {
        assert!(Config::build(false, None, |_| None).0.media.music_panel);
        let (config, warnings) = Config::build(false, Some("[media]\nmusic_panel = false"), |_| None);
        assert!(warnings.is_empty());
        assert!(!config.media.music_panel);
        assert_eq!(config.values()["media"]["music_panel"], false);
        assert_eq!(Config::build(false, Some("[media]\nwrong = true"), |_| None).1, ["unknown config keys ignored: media.wrong"]);
    }
}

#[cfg(test)]
mod brightness_tests {
    use super::*;
    use crate::brightness::Control;

    #[test]
    fn brightness_control_parses_and_rejects_unknown_modes() {
        assert_eq!(Config::build(false, None, |_| None).0.display.brightness_control, Control::Auto);
        for (name, expected) in [("auto", Control::Auto), ("backlight", Control::Backlight), ("software", Control::Software)] {
            let text = format!("[display]\nbrightness_control = '{name}'");
            let config = Config::parse_checked(false, &text, |_| None).unwrap();
            assert_eq!(config.display.brightness_control, expected);
            assert_eq!(config.values()["display"]["brightness_control"], name);
        }
        assert!(Config::parse_checked(false, "[display]\nbrightness_control = 'bad'", |_| None).is_err());
    }
}

#[cfg(test)]
mod classic_window_tests {
    use super::*;

    #[test]
    fn classic_start_icon_defaults_empty_and_parses() {
        assert_eq!(Config::build(false, None, |_| None).0.classic.start_icon, "");
        let config = Config::parse_checked(false, "[classic]\nstart_icon = 'my-logo'", |_| None).unwrap();
        assert_eq!(config.classic.start_icon, "my-logo");
        assert_eq!(config.values()["classic"]["start_icon"], "my-logo");
        assert!(Config::parse_checked(false, "[classic]\nstart_icon = 3", |_| None).is_err());
    }
    #[test]
    fn classic_open_maximized_defaults_parses_and_validates() {
        assert!(Config::build(false, None, |_| None).0.classic.open_maximized);
        for enabled in [true, false] {
            let text = format!("[classic]\nopen_maximized = {enabled}");
            let config = Config::parse_checked(false, &text, |_| None).unwrap();
            assert_eq!(config.classic.open_maximized, enabled);
            assert_eq!(config.values()["classic"]["open_maximized"], enabled);
        }
        assert!(Config::parse_checked(false, "[classic]\nopen_maximized = 'yes'", |_| None).is_err());
    }
}
