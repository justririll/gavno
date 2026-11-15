//! Runtime configuration.
//!
//! Phase 1 reads a handful of environment variables. A KDL config file with
//! live reload replaces this later; keep the field names stable.

use smithay::input::keyboard::ModifiersState;

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl Config {
    pub fn from_env(nested: bool) -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Self {
            mod_key: match var("DS_MOD").as_deref() {
                Some("alt") => ModKey::Alt,
                Some("super") => ModKey::Super,
                _ if nested => ModKey::Alt,
                _ => ModKey::Super,
            },
            terminal: var("DS_TERMINAL").unwrap_or_else(|| "foot".into()),
            launcher: var("DS_LAUNCHER"),
            internal_scale: var("DS_SCALE").and_then(|s| s.parse().ok()).unwrap_or(1.75),
            xkb_layout: var("DS_XKB_LAYOUT").unwrap_or_else(|| "us,ru".into()),
            xkb_variant: var("DS_XKB_VARIANT").unwrap_or_default(),
            xkb_options: Some(var("DS_XKB_OPTIONS").unwrap_or_else(|| "grp:alt_shift_toggle".into())),
            gap: 12,
        }
    }
}
