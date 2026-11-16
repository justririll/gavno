use serde_json::Value;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputChange {
    pub output: String,
    pub mode: Option<ModeChange>,
    pub scale: Option<f64>,
    pub hdr: Option<bool>,
}
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeChange {
    pub w: i32,
    pub h: i32,
    /// Millihertz, matching the `outputs` response and wl_output.
    pub refresh: Option<i32>,
}
impl OutputChange {
    pub fn parse(params: &Value) -> Result<Self, String> {
        if params
            .as_object()
            .is_none_or(|p| p.values().any(Value::is_null))
        {
            return Err("settings fields cannot be null".into());
        }
        let change: Self = serde_json::from_value(params.clone()).map_err(|e| e.to_string())?;
        if change.output.is_empty() || change.output.len() > 256 {
            return Err("invalid connector name".into());
        }
        if change.scale.is_some_and(|s| {
            !s.is_finite()
                || !(0.5..=3.0).contains(&s)
                || (s * 20.0 - (s * 20.0).round()).abs() > 0.000001
        }) {
            return Err("scale must be 0.5..=3.0 in steps of 0.05".into());
        }
        if change.mode.as_ref().is_some_and(|m| {
            !(1..=65535).contains(&m.w)
                || !(1..=65535).contains(&m.h)
                || m.refresh.is_some_and(|r| r <= 0)
        }) {
            return Err("invalid output mode".into());
        }
        if change.mode.is_none() && change.scale.is_none() && change.hdr.is_none() {
            return Err("no output changes supplied".into());
        }
        Ok(change)
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyboardChange {
    pub layout: String,
    #[serde(default)]
    pub variant: String,
    #[serde(default)]
    pub options: String,
}
impl KeyboardChange {
    pub fn parse(params: &Value) -> Result<Self, String> {
        if params
            .as_object()
            .is_none_or(|p| p.values().any(Value::is_null))
        {
            return Err("settings fields cannot be null".into());
        }
        let change: Self = serde_json::from_value(params.clone()).map_err(|e| e.to_string())?;
        let valid = |s: &str| {
            s.len() <= 1024
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_,:-+".contains(&b))
        };
        if change.layout.is_empty()
            || change.layout.split(',').count() > 4
            || change.layout.split(',').any(str::is_empty)
            || !valid(&change.layout)
            || !valid(&change.variant)
            || !valid(&change.options)
        {
            return Err(
                "invalid keyboard layout, variant or options (maximum four layouts)".into(),
            );
        }
        Ok(change)
    }
    /// Run on a worker: resolving XKB rules and symbols reads system files.
    pub fn compile(&self) -> Result<String, String> {
        use smithay::input::keyboard::xkb;
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            &self.layout,
            &self.variant,
            Some(self.options.clone()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or("cannot compile keyboard keymap")?;
        Ok(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
    }
}

pub fn output_state(gv: &crate::Gavno, output: &smithay::output::Output) -> Value {
    let props = output.physical_properties();
    let hdr = crate::backend::udev::hdr_status(&gv.backend, output);
    let mode_json = |m: smithay::output::Mode| serde_json::json!({"w":m.size.w,"h":m.size.h,"refresh":m.refresh,"preferred":crate::backend::udev::mode_preferred(output, m)});
    serde_json::json!({"name":output.name(),"make":props.make,"model":props.model,
        "enabled":gv.space.outputs().any(|o| o==output),
        "mode":output.current_mode().map(mode_json),"modes":output.modes().into_iter().map(mode_json).collect::<Vec<_>>(),
        "scale":output.current_scale().fractional_scale(),"position":{"x":output.current_location().x,"y":output.current_location().y},
        "hdr":{"supported":hdr.as_ref().is_some_and(|h| h.supported),"enabled":hdr.as_ref().is_some_and(|h| h.enabled),"pending":hdr.and_then(|h| h.pending)},
        "internal":output.name().starts_with("eDP-")})
}

pub fn keyboard_state(gv: &mut crate::Gavno) -> Result<Value, String> {
    let keyboard = gv.seat.get_keyboard().ok_or("seat has no keyboard")?;
    let (active, names) = keyboard.with_xkb_state(gv, |ctx| {
        let xkb = ctx.xkb().lock().unwrap_or_else(|e| e.into_inner());
        (
            xkb.active_layout().0,
            xkb.layouts()
                .map(|l| xkb.layout_name(l).to_owned())
                .collect::<Vec<_>>(),
        )
    });
    Ok(
        serde_json::json!({"layout":gv.config.xkb_layout,"variant":gv.config.xkb_variant,"options":gv.config.xkb_options,
        "active_group":active,"group_names":names}),
    )
}

pub fn set_output(gv: &mut crate::Gavno, change: OutputChange) -> Result<Value, String> {
    let output = gv
        .space
        .outputs()
        .find(|o| o.name() == change.output)
        .cloned()
        .ok_or("unknown or disabled output")?;
    let mode = change
        .mode
        .map(|m| {
            output
                .modes()
                .into_iter()
                .filter(|mode| {
                    mode.size.w == m.w
                        && mode.size.h == m.h
                        && m.refresh.is_none_or(|r| r == mode.refresh)
                })
                .max_by_key(|m| m.refresh)
                .ok_or("mode unavailable")
        })
        .transpose()?;
    if change.hdr == Some(true)
        && !crate::backend::udev::hdr_status(&gv.backend, &output).is_some_and(|h| h.supported)
    {
        return Err("HDR unsupported on output".into());
    }
    let previous_mode = output.current_mode();
    let previous_scale = output.current_scale();
    if let Some(mode) = mode {
        crate::backend::udev::set_mode(gv, &output, mode)?;
    }
    if let Some(scale) = change.scale {
        output.change_current_state(
            None,
            None,
            Some(smithay::output::Scale::Fractional(scale)),
            None,
        );
    }
    if mode.is_some() || change.scale.is_some() {
        if let Err(error) = crate::backend::udev::refresh_output_geometry(gv, &output) {
            if let Some(previous) = previous_mode {
                crate::backend::udev::set_mode(gv, &output, previous)
                    .map_err(|restore| format!("{error}; restoring mode failed: {restore}"))?;
            }
            output.change_current_state(None, None, Some(previous_scale), None);
            return Err(error);
        }
    }
    if let Some(hdr) = change.hdr {
        if matches!(gv.backend, crate::backend::Backend::Udev(_)) {
            crate::backend::udev::set_hdr(gv, &output.name(), hdr)?;
        }
    }
    gv.output_settings_changed(&output);
    Ok(output_state(gv, &output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn validates_output_and_keyboard_arguments() {
        for params in [
            json!({}),
            json!({"output":"eDP-1"}),
            json!({"output":"eDP-1","scale":0.49}),
            json!({"output":"eDP-1","scale":3.05}),
            json!({"output":"eDP-1","scale":1.73}),
            json!({"output":"eDP-1","hdr":1}),
            json!({"output":"eDP-1","mode":{"w":0,"h":900}}),
            json!({"output":"eDP-1","mode":{"w":1600,"h":900,"refresh":-1}}),
            json!({"output":"eDP-1","enabled":false}),
        ] {
            assert!(OutputChange::parse(&params).is_err(), "{params}");
        }
        for scale in [0.5, 1.0, 1.25, 1.75, 2.95, 3.0] {
            assert!(OutputChange::parse(&json!({"output":"eDP-1","scale":scale})).is_ok());
        }
        for params in [
            json!({}),
            json!({"layout":""}),
            json!({"layout":"us,,,,"}),
            json!({"layout":"us\u{0}"}),
            json!({"layout":"us","options":42}),
            json!({"layout":"us","variant":"../../foo"}),
        ] {
            assert!(KeyboardChange::parse(&params).is_err(), "{params}");
        }
        assert!(
            KeyboardChange::parse(&json!({"layout":"us,ru","options":"grp:win_space_toggle"}))
                .is_ok()
        );
    }
    #[test]
    fn scale_refits_floating_fixed_and_hidden_windows() {
        use smithay::{
            output::{Mode, Output, PhysicalProperties, Subpixel},
            utils::Rectangle,
        };
        let mut h = crate::state::tests::Harness::new();
        let output = Output::new(
            "eDP-1".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: (1600, 900).into(),
                refresh: 60000,
            }),
            None,
            None,
            Some((0, 0).into()),
        );
        h.gv.space.map_output(&output, (0, 0));
        let floating = h.window();
        h.gv.map_new_window(floating.clone());
        h.commit_size(&floating, 500, 200);
        h.gv.configure_slot(
            &floating,
            Rectangle::new((990, 480).into(), (500, 200).into()),
            crate::window_mode::WindowMode::Floating,
        );
        let fixed = h.window();
        h.gv.map_new_window(fixed.clone());
        h.gv.set_window_mode(
            &fixed,
            crate::window_mode::WindowMode::Maximized,
            Some(output.clone()),
        );
        let hidden = h.window();
        h.gv.map_new_window(hidden.clone());
        h.commit_size(&hidden, 500, 200);
        h.gv.configure_slot(
            &hidden,
            Rectangle::new((990, 480).into(), (500, 200).into()),
            crate::window_mode::WindowMode::Floating,
        );
        h.gv.space.unmap_elem(&hidden);
        let before = h.gv.config.internal_scale;
        let result = set_output(
            &mut h.gv,
            OutputChange::parse(&json!({"output":"eDP-1","scale":2.0})).unwrap(),
        )
        .unwrap();
        assert_eq!(result["scale"], 2.0);
        assert_eq!(output.current_scale().fractional_scale(), 2.0);
        assert_eq!(
            h.gv.config.internal_scale, before,
            "live scale must not alter configuration"
        );
        let area = h.gv.work_area(&output);
        let location = h.gv.space.element_location(&floating).unwrap();
        assert!(area.contains_rect(Rectangle::new(location, (500, 200).into())));
        assert!(h.gv.space.element_location(&hidden).is_none());
        let hidden_loc = h.gv.workspaces.location(&hidden).unwrap();
        assert!(area.contains_rect(Rectangle::new(hidden_loc, (500, 200).into())));
        let size = fixed
            .toplevel()
            .unwrap()
            .with_pending_state(|s| s.size)
            .unwrap();
        assert!(size.w <= area.size.w && size.h <= area.size.h);
        let old = output.current_mode();
        assert!(set_output(
            &mut h.gv,
            OutputChange::parse(&json!({"output":"eDP-1","scale":1.0,"mode":{"w":1,"h":1}}))
                .unwrap()
        )
        .is_err());
        assert_eq!(output.current_mode(), old);
        assert_eq!(output.current_scale().fractional_scale(), 2.0);
        assert!(set_output(
            &mut h.gv,
            OutputChange::parse(&json!({"output":"eDP-1","scale":1.0,"hdr":true})).unwrap()
        )
        .is_err());
        assert_eq!(output.current_scale().fractional_scale(), 2.0);
    }
    #[test]
    fn unknown_layout_does_not_change_keymap() {
        let mut h = crate::state::tests::Harness::new();
        let before = keyboard_state(&mut h.gv).unwrap();
        let change =
            KeyboardChange::parse(&json!({"layout":"gavno_unknown_layout_12345"})).unwrap();
        assert!(change.compile().is_err());
        assert_eq!(keyboard_state(&mut h.gv).unwrap(), before);
        let good = KeyboardChange::parse(&json!({"layout":"us","options":"grp:win_space_toggle"}))
            .unwrap();
        let keymap = good.compile().unwrap();
        h.gv.apply_keyboard_keymap(good.layout, good.variant, good.options, keymap)
            .unwrap();
        assert_eq!(keyboard_state(&mut h.gv).unwrap()["layout"], "us");
    }
}
