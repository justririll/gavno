//! Multi-monitor rules that do not need the DRM device: when the laptop panel
//! should be lit, and where outputs go in the global layout.

use smithay::utils::{Logical, Point, Rectangle, Size};

/// What decides whether the internal panel is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelInputs {
    /// `[display] internal_off_with`; 0 never turns the panel off.
    pub threshold: u32,
    /// External outputs currently lit.
    pub externals_on: usize,
    pub lid_closed: bool,
    /// `[outputs.<panel>] enabled`, which wins over the automatic rule.
    pub explicit: Option<bool>,
}

/// The panel is never turned off without another lit output: with no external
/// output it stays on, even with a closed lid (logind decides about suspend).
pub fn panel_wanted(inputs: PanelInputs) -> bool {
    if inputs.externals_on == 0 {
        return true;
    }
    if let Some(enabled) = inputs.explicit {
        return enabled;
    }
    if inputs.lid_closed {
        return false;
    }
    inputs.threshold == 0 || inputs.externals_on < inputs.threshold as usize
}

/// Where a new output goes: its configured position, unless that overlaps an
/// existing output, else to the right of everything at the top edge.
pub fn place(
    requested: Option<[i32; 2]>,
    size: Size<i32, Logical>,
    existing: &[Rectangle<i32, Logical>],
) -> Point<i32, Logical> {
    if let Some([x, y]) = requested {
        let rect = Rectangle::new((x, y).into(), size);
        if !existing.iter().any(|other| other.overlaps(rect)) {
            return (x, y).into();
        }
    }
    let x = existing.iter().map(|r| r.loc.x + r.size.w).max().unwrap_or(0);
    (x, 0).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(externals_on: usize) -> PanelInputs {
        PanelInputs { threshold: 2, externals_on, lid_closed: false, explicit: None }
    }

    #[test]
    fn external_count_turns_the_panel_off_at_the_threshold() {
        assert!(panel_wanted(inputs(0)));
        assert!(panel_wanted(inputs(1)));
        assert!(!panel_wanted(inputs(2)));
        assert!(!panel_wanted(inputs(3)));
        assert!(panel_wanted(PanelInputs { threshold: 1, ..inputs(0) }));
        assert!(!panel_wanted(PanelInputs { threshold: 1, ..inputs(1) }));
    }

    #[test]
    fn zero_threshold_never_turns_the_panel_off_unless_the_lid_closes() {
        assert!(panel_wanted(PanelInputs { threshold: 0, ..inputs(4) }));
        assert!(!panel_wanted(PanelInputs { threshold: 0, lid_closed: true, ..inputs(1) }));
    }

    #[test]
    fn closed_lid_needs_another_output() {
        assert!(!panel_wanted(PanelInputs { lid_closed: true, ..inputs(1) }));
        assert!(panel_wanted(PanelInputs { lid_closed: true, ..inputs(0) }));
    }

    #[test]
    fn explicit_setting_wins_but_never_leaves_no_output() {
        assert!(panel_wanted(PanelInputs { explicit: Some(true), lid_closed: true, ..inputs(3) }));
        assert!(!panel_wanted(PanelInputs { explicit: Some(false), ..inputs(1) }));
        assert!(panel_wanted(PanelInputs { explicit: Some(false), ..inputs(0) }));
    }

    #[test]
    fn placement_honours_free_positions_and_falls_back_to_the_right() {
        let panel = Rectangle::new((0, 0).into(), (1600, 1000).into());
        let size = Size::from((1920, 1080));
        assert_eq!(place(None, size, &[]), Point::from((0, 0)));
        assert_eq!(place(None, size, &[panel]), Point::from((1600, 0)));
        assert_eq!(place(Some([-1920, 0]), size, &[panel]), Point::from((-1920, 0)));
        assert_eq!(place(Some([100, 0]), size, &[panel]), Point::from((1600, 0)));
    }
}
