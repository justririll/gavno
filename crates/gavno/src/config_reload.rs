//! Explicit, transactional reload: disk/XKB work is prepared on a one-shot
//! worker; calloop applies the result without reading files or waiting on it.
use crate::{
    config::Config,
    config_schema::SCHEMA,
    ipc::settings::{KeyboardChange, ModeChange, OutputChange},
    Gavno,
};
use serde_json::{json, Value};
use smithay::output::Output;

pub(crate) struct Prepared {
    pub config: Config,
    keymap: Option<String>,
}
impl Prepared {
    pub fn read(nested: bool, previous: &Config) -> Result<Self, String> {
        Self::new(Config::load_checked(nested)?, previous)
    }
    pub fn new(config: Config, previous: &Config) -> Result<Self, String> {
        let keymap = if config.xkb_layout != previous.xkb_layout
            || config.xkb_variant != previous.xkb_variant
            || config.xkb_options != previous.xkb_options
        {
            Some(KeyboardChange::parse(&json!({"layout":config.xkb_layout,"variant":config.xkb_variant,"options":config.xkb_options.clone().unwrap_or_default()}))?.compile()?)
        } else {
            None
        };
        Ok(Self { config, keymap })
    }
}
fn at<'a>(root: &'a Value, parts: &[&str]) -> &'a Value {
    parts.iter().fold(root, |v, key| &v[*key])
}
/// Return concrete paths, including quoted connector names, only for known keys.
pub(crate) fn changed(previous: &Config, next: &Config) -> (Vec<String>, Vec<String>) {
    let old = previous.values();
    let new = next.values();
    let mut applied = Vec::new();
    let mut deferred = Vec::new();
    for key in SCHEMA {
        let parts: Vec<_> = key.path.split('.').collect();
        let paths: Vec<Vec<&str>> = if parts.get(1) == Some(&"*") {
            let mut names: Vec<_> = previous
                .outputs
                .keys()
                .chain(next.outputs.keys())
                .map(String::as_str)
                .collect();
            names.sort_unstable();
            names.dedup();
            names
                .into_iter()
                .map(|name| vec!["outputs", name, parts[2]])
                .collect()
        } else {
            vec![parts]
        };
        for parts in paths {
            if at(&old, &parts) == at(&new, &parts) {
                continue;
            }
            let path = if parts.first() == Some(&"outputs") {
                format!("outputs.{}.{}", json!(parts[1]), parts[2])
            } else {
                key.path.into()
            };
            if key.live {
                applied.push(path);
            } else {
                deferred.push(path);
            }
        }
    }
    (applied, deferred)
}
fn mode_change(mode: smithay::output::Mode) -> ModeChange {
    ModeChange {
        w: mode.size.w,
        h: mode.size.h,
        refresh: Some(mode.refresh),
    }
}
fn output_plan(gv: &Gavno, next: &Config, output: &Output) -> Result<OutputChange, String> {
    let internal = ["eDP-", "LVDS-", "DSI-"]
        .iter()
        .any(|p| output.name().starts_with(p));
    let (requested, scale, hdr, _) = next.output_settings(&output.name(), internal);
    let modes = output.modes();
    let candidates: Vec<_> = modes
        .iter()
        .map(|m| crate::hdr::mode::Candidate {
            width: m.size.w as u16,
            height: m.size.h as u16,
            refresh: m.refresh,
            preferred: crate::backend::udev::mode_preferred(output, *m),
        })
        .collect();
    let chosen = crate::hdr::mode::select(&candidates, requested)
        .or_else(|| crate::hdr::mode::select(&candidates, None));
    let mode = chosen.and_then(|i| modes.get(i)).copied();
    let status = crate::backend::udev::hdr_status(&gv.backend, output);
    if hdr && !status.as_ref().is_some_and(|s| s.supported) {
        return Err(format!("{}: HDR unsupported", output.name()));
    }
    Ok(OutputChange {
        output: output.name(),
        mode: mode
            .filter(|m| Some(*m) != output.current_mode())
            .map(mode_change),
        scale: (scale != output.current_scale().fractional_scale()).then_some(scale),
        hdr: (hdr
            != status
                .as_ref()
                .is_some_and(|s| s.pending.unwrap_or(s.enabled)))
        .then_some(hdr),
    })
}

pub(crate) fn apply(gv: &mut Gavno, prepared: Prepared) -> Result<Value, String> {
    let Prepared { mut config, keymap } = prepared;
    let (applied, needs_relogin) = changed(&gv.config, &config);
    let output_changes = applied
        .iter()
        .any(|k| k.starts_with("output.") || k.starts_with("outputs."));
    let outputs: Vec<_> = gv.space.outputs().cloned().collect();
    let mut plans = Vec::new();
    if output_changes {
        for output in &outputs {
            let change = output_plan(gv, &config, output)?;
            if change.mode.is_none() && change.scale.is_none() && change.hdr.is_none() {
                continue;
            }
            let status = crate::backend::udev::hdr_status(&gv.backend, output);
            let rollback = OutputChange {
                output: output.name(),
                mode: output.current_mode().map(mode_change),
                scale: Some(output.current_scale().fractional_scale()),
                hdr: status.map(|s| s.pending.unwrap_or(s.enabled)),
            };
            plans.push((change, rollback));
        }
    }
    let mut undo = Vec::new();
    for (change, rollback) in plans {
        if let Err(error) = crate::ipc::settings::set_output(gv, change) {
            for rollback in undo.into_iter().rev() {
                if let Err(restore) = crate::ipc::settings::set_output(gv, rollback) {
                    tracing::error!(%restore,"cannot restore output after failed reload");
                }
            }
            return Err(error);
        }
        undo.push(rollback);
    }
    if let Some(keymap) = keymap {
        if let Err(error) = gv.apply_keyboard_keymap(
            config.xkb_layout.clone(),
            config.xkb_variant.clone(),
            config.xkb_options.clone().unwrap_or_default(),
            keymap,
        ) {
            for rollback in undo.into_iter().rev() {
                let _ = crate::ipc::settings::set_output(gv, rollback);
            }
            return Err(error);
        }
    }
    let lock_changed = gv.config.lock.after_idle_secs != config.lock.after_idle_secs;
    let power_changed = gv.config.display.off_after_idle_secs != config.display.off_after_idle_secs;
    let icons_changed = gv.config.dock.icon_theme != config.dock.icon_theme;
    let white_changed = gv.config.sdr_white_nits != config.sdr_white_nits;
    let pins_changed = gv.config.dock.pinned != config.dock.pinned;
    let classic_changed = gv.config.classic != config.classic;
    // These startup-only settings are not considered effective before re-login.
    config.autostart = gv.config.autostart.clone();
    config.polkit_agent = gv.config.polkit_agent.clone();
    for (name, output) in &mut config.outputs {
        output.enabled = gv.config.outputs.get(name).and_then(|o| o.enabled);
    }
    for (name, old) in &gv.config.outputs {
        if old.enabled.is_some() {
            config.outputs.entry(name.clone()).or_default().enabled = old.enabled;
        }
    }
    gv.config = config;
    gv.sync_brightness();
    if let Some(keyboard) = gv.seat.get_keyboard() {
        keyboard.change_repeat_info(gv.config.repeat_rate, gv.config.repeat_delay);
    }
    gv.reload_features();
    if classic_changed && gv.classic.desktop.active {
        gv.classic.desktop.widgets.configure(&gv.config.classic.widgets);
        gv.request_desktop_scan();
        gv.queue_redraw_all();
    }
    if icons_changed || pins_changed {
        gv.dock.configuration_changed();
    }
    if icons_changed && gv.features.on(crate::module::Feature::Media) {
        gv.media_generation = gv.media_generation.wrapping_add(1);
        if let Some(worker) = gv.now_playing.borrow_mut().worker.take() {
            let _ = worker.send(crate::media::mpris::Request::Disconnected);
        }
        *gv.now_playing.borrow_mut() = Default::default();
    }
    let default_tiling = gv.config.tiling.default && gv.features.on(crate::module::Feature::Tiling);
    gv.workspaces.default_tiling = default_tiling;
    for owner in &mut gv.workspaces.outputs {
        owner.list.default_tiling = default_tiling;
        for ws in &mut owner.list.items {
            if ws.windows.is_empty() {
                ws.tiling = default_tiling;
            }
        }
    }
    if white_changed {
        crate::backend::udev::set_sdr_white(gv, gv.config.sdr_white_nits);
    }
    if gv.config.display.off_after_idle_secs == 0 { gv.power_on_monitors("config reload"); }
    if power_changed {
        let inhibited = gv.protocols.idle.is_inhibited();
        gv.refresh_power_idle(true, inhibited);
    }
    if lock_changed {
        let inhibited = gv.protocols.idle.is_inhibited();
        gv.refresh_lock_idle(true, inhibited);
    }
    gv.arrange_layers_all();
    gv.refresh_notifications();
    gv.refresh_media();
    Ok(json!({"applied":applied,"needs_relogin":needs_relogin}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reload_changes_registry_resets_and_restores_live_settings() {
        let mut h = crate::state::tests::Harness::new();
        let next = Config::parse_checked(true,"gap=20\n[dock]\nautohide=true\npinned=['foot']\n[bar]\nenabled=false\n[modules]\ndock=false\nanimations=false", |_|None).unwrap();
        let result = apply(
            &mut h.gv,
            Prepared::new(next, &Config::build(true, None, |_| None).0).unwrap(),
        )
        .unwrap();
        assert_eq!(h.gv.config.gap, 20);
        assert!(h.gv.config.dock.autohide);
        assert!(!h.gv.features.on(crate::module::Feature::Dock));
        assert!(!h.gv.features.on(crate::module::Feature::Bar));
        assert!(!h.gv.features.on(crate::module::Feature::Animations));
        assert!(h.gv.features.reset_count[crate::module::Feature::Dock as usize] > 0);
        assert!(result["applied"]
            .as_array()
            .unwrap()
            .contains(&json!("dock.autohide")));
        let next = Config::build(true, None, |_| None).0;
        let prepared = Prepared::new(next, &h.gv.config).unwrap();
        apply(&mut h.gv, prepared).unwrap();
        assert!(h.gv.features.on(crate::module::Feature::Dock));
        assert!(h.gv.features.on(crate::module::Feature::Bar));
    }
    #[test]
    fn invalid_text_or_keymap_cannot_prepare_a_reload() {
        let old = Config::build(true, None, |_| None).0;
        let error = Config::parse_checked(true, "[dock]\nenabled = 'wrong'", |_| None).unwrap_err();
        assert!(error.contains("line 2"));
        assert!(error.contains("column"));
        let next = Config::build(
            true,
            Some("[keyboard]\nlayout='gavno_invalid_layout_reload'"),
            |_| None,
        )
        .0;
        assert!(Prepared::new(next, &old).is_err());
    }
}

#[cfg(test)]
mod deferred_tests {
    use super::*;
    #[test]
    fn reports_startup_keys_and_defers_ipc_shutdown_until_after_reply() {
        let mut h = crate::state::tests::Harness::new();
        let old = h.gv.config.clone();
        let next=Config::parse_checked(true,"autostart=['false']\npolkit_agent='other-agent'\n[outputs.'eDP-1']\nenabled=false\n[modules]\nipc=false",|_|None).unwrap();
        let prepared = Prepared::new(next, &old).unwrap();
        let result = apply(&mut h.gv, prepared).unwrap();
        for key in ["autostart", "polkit_agent", "outputs.\"eDP-1\".enabled"] {
            assert!(
                result["needs_relogin"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(key)),
                "{key}"
            );
        }
        assert_eq!(h.gv.config.autostart, old.autostart);
        assert_eq!(h.gv.config.polkit_agent, old.polkit_agent);
        assert!(
            h.gv.features.on(crate::module::Feature::Ipc),
            "reply must be deliverable"
        );
        h.event_loop
            .dispatch(std::time::Duration::ZERO, &mut h.gv)
            .unwrap();
        assert!(!h.gv.features.on(crate::module::Feature::Ipc));
    }
}

#[cfg(test)]
mod media_generation_tests {
    use super::*;
    #[test]
    fn stopped_media_worker_cannot_overwrite_a_new_generation() {
        let mut h = crate::state::tests::Harness::new();
        let stale = h.gv.media_events().unwrap();
        let next = Config::build(true, Some("[modules]\nmedia=false"), |_| None).0;
        let prepared = Prepared::new(next, &h.gv.config).unwrap();
        apply(&mut h.gv, prepared).unwrap();
        let next = Config::build(true, None, |_| None).0;
        let prepared = Prepared::new(next, &h.gv.config).unwrap();
        apply(&mut h.gv, prepared).unwrap();
        h.gv.now_playing.borrow_mut().snapshot.player = Some(crate::media::Player {
            name: "new player".into(),
            ..Default::default()
        });
        stale.send(crate::media::Snapshot::default()).unwrap();
        h.event_loop
            .dispatch(std::time::Duration::ZERO, &mut h.gv)
            .unwrap();
        assert_eq!(
            h.gv.now_playing
                .borrow()
                .snapshot
                .player
                .as_ref()
                .unwrap()
                .name,
            "new player"
        );
    }
}
