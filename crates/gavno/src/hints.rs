//! The single bind list behind the top-bar hint strip and the cheat
//! sheet. Each entry is one visible hint: a chord set, the action each chord
//! dispatches to, a short label, a priority and the context it belongs to.
//!
//! Priorities start at 1 for the hints that must survive the longest; the bar
//! drops the highest numbers first when it runs out of room.

use crate::{input::Action, snap::Direction};
use smithay::input::keyboard::keysyms;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintContext {
    Code,
    Resize,
    Windows,
    Overview,
    Popover,
}

/// Modifiers of a chord. `main` is the configured Mod key, `extra` is the
/// additional modifier used by the nested swap variant (Super while Alt is
/// Mod, Alt while Super is Mod).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChordMods {
    pub main: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub extra: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HintAction {
    /// A compositor bind; `Gavno::bind_for` must return it.
    Bind(Action),
    /// Resize mode moves the active edge by its normal (10 px) or big (50 px)
    /// step.
    ResizeEdge(Direction, bool),
    /// Resize mode cycles the active edge.
    ResizeNextEdge(bool),
    /// Resize mode leaves.
    ResizeDone,
    MoveFloating,
    WindowLetter,
    WindowCancel,
    OverviewKey,
    PopoverKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    pub mods: ChordMods,
    /// XKB Latin keysym, like the other binds.
    pub sym: u32,
    pub action: HintAction,
}

#[derive(Clone, Copy, Debug)]
pub struct BindHint {
    pub context: HintContext,
    /// Key text without the Mod prefix, for example `HJKL` or `Ctrl+HJKL`.
    pub keys: &'static str,
    pub label: &'static str,
    /// 1 = keep the longest, larger numbers are dropped first.
    pub priority: u8,
    /// Every chord of this hint, checked against the dispatcher in tests and
    /// used by the cheat sheet.
    pub chords: &'static [Chord],
}

const MAIN: ChordMods = ChordMods {
    main: true,
    ctrl: false,
    shift: false,
    alt: false,
    extra: false,
};
const MAIN_CTRL: ChordMods = ChordMods {
    main: true,
    ctrl: true,
    shift: false,
    alt: false,
    extra: false,
};
const MAIN_SHIFT: ChordMods = ChordMods {
    main: true,
    ctrl: false,
    shift: true,
    alt: false,
    extra: false,
};
#[cfg(test)]
pub(crate) const MAIN_FOR_TESTS: ChordMods = MAIN;
const NONE: ChordMods = ChordMods {
    main: false,
    ctrl: false,
    shift: false,
    alt: false,
    extra: false,
};
const SHIFT: ChordMods = ChordMods {
    main: false,
    ctrl: false,
    shift: true,
    alt: false,
    extra: false,
};
const CTRL: ChordMods = ChordMods {
    main: false,
    ctrl: true,
    shift: false,
    alt: false,
    extra: false,
};

const fn bind(mods: ChordMods, sym: u32, action: Action) -> Chord {
    Chord {
        mods,
        sym,
        action: HintAction::Bind(action),
    }
}

const fn resize(mods: ChordMods, sym: u32, dir: Direction, big: bool) -> Chord {
    Chord {
        mods,
        sym,
        action: HintAction::ResizeEdge(dir, big),
    }
}

/// Code mode, in bar order.
const CODE_HINTS: &[BindHint] = &[
    BindHint {
        context: HintContext::Code,
        keys: "HJKL",
        label: "focus",
        priority: 1,
        chords: &[
            bind(
                MAIN,
                keysyms::KEY_h,
                Action::FocusDirection(Direction::Left),
            ),
            bind(
                MAIN,
                keysyms::KEY_j,
                Action::FocusDirection(Direction::Down),
            ),
            bind(MAIN, keysyms::KEY_k, Action::FocusDirection(Direction::Up)),
            bind(
                MAIN,
                keysyms::KEY_l,
                Action::FocusDirection(Direction::Right),
            ),
        ],
    },
    BindHint {
        context: HintContext::Code,
        keys: "Ctrl+HJKL",
        label: "move",
        priority: 2,
        chords: &[
            bind(MAIN_CTRL, keysyms::KEY_h, Action::Snap(Direction::Left)),
            bind(MAIN_CTRL, keysyms::KEY_j, Action::Snap(Direction::Down)),
            bind(MAIN_CTRL, keysyms::KEY_k, Action::Snap(Direction::Up)),
            bind(MAIN_CTRL, keysyms::KEY_l, Action::Snap(Direction::Right)),
        ],
    },
    BindHint {
        context: HintContext::Code,
        keys: "Shift+HJKL",
        label: "resize",
        priority: 3,
        chords: &[
            bind(MAIN_SHIFT, keysyms::KEY_h, Action::Resize(Direction::Left)),
            bind(MAIN_SHIFT, keysyms::KEY_j, Action::Resize(Direction::Down)),
            bind(MAIN_SHIFT, keysyms::KEY_k, Action::Resize(Direction::Up)),
            bind(MAIN_SHIFT, keysyms::KEY_l, Action::Resize(Direction::Right)),
        ],
    },
    BindHint {
        context: HintContext::Code,
        keys: "G",
        label: "hints",
        priority: 7,

        chords: &[bind(MAIN, keysyms::KEY_g, Action::WindowHints)],
    },
    BindHint {
        context: HintContext::Code,
        keys: "R",
        label: "resize mode",
        priority: 4,
        chords: &[bind(MAIN, keysyms::KEY_r, Action::ResizeMode)],
    },
    BindHint {
        context: HintContext::Code,
        keys: "A",
        label: "overview",
        priority: 5,
        chords: &[bind(MAIN, keysyms::KEY_a, Action::ToggleOverview)],
    },
    BindHint {
        context: HintContext::Code,
        keys: "1-9",
        label: "workspaces",
        priority: 6,
        chords: &[
            bind(MAIN, keysyms::KEY_1, Action::Workspace(0)),
            bind(MAIN, keysyms::KEY_2, Action::Workspace(1)),
            bind(MAIN, keysyms::KEY_3, Action::Workspace(2)),
            bind(MAIN, keysyms::KEY_4, Action::Workspace(3)),
            bind(MAIN, keysyms::KEY_5, Action::Workspace(4)),
            bind(MAIN, keysyms::KEY_6, Action::Workspace(5)),
            bind(MAIN, keysyms::KEY_7, Action::Workspace(6)),
            bind(MAIN, keysyms::KEY_8, Action::Workspace(7)),
            bind(MAIN, keysyms::KEY_9, Action::Workspace(8)),
        ],
    },
    BindHint {
        context: HintContext::Code,
        keys: "/",
        label: "cheat sheet",
        priority: 8,

        chords: &[bind(MAIN, keysyms::KEY_slash, Action::CheatSheet)],
    },
    BindHint {
        context: HintContext::Code,
        keys: "Shift+N",
        label: "Wi-Fi",
        priority: 10,
        chords: &[bind(
            MAIN_SHIFT,
            keysyms::KEY_n,
            Action::PanelControls(crate::panel_controls::Kind::Wifi),
        )],
    },
    BindHint {
        context: HintContext::Code,
        keys: "Shift+B",
        label: "Bluetooth",
        priority: 11,
        chords: &[bind(
            MAIN_SHIFT,
            keysyms::KEY_b,
            Action::PanelControls(crate::panel_controls::Kind::Bluetooth),
        )],
    },
    BindHint {
        context: HintContext::Code,
        keys: "Shift+V",
        label: "mixer",
        priority: 12,
        chords: &[bind(
            MAIN_SHIFT,
            keysyms::KEY_v,
            Action::PanelControls(crate::panel_controls::Kind::Audio),
        )],
    },
];

/// Resize mode, in bar order.
const RESIZE_HINTS: &[BindHint] = &[
    BindHint {
        context: HintContext::Resize,
        keys: "arrows/HJKL",
        label: "edge",
        priority: 1,
        chords: &[
            resize(NONE, keysyms::KEY_Left, Direction::Left, false),
            resize(NONE, keysyms::KEY_Right, Direction::Right, false),
            resize(NONE, keysyms::KEY_Up, Direction::Up, false),
            resize(NONE, keysyms::KEY_Down, Direction::Down, false),
            resize(NONE, keysyms::KEY_h, Direction::Left, false),
            resize(NONE, keysyms::KEY_j, Direction::Down, false),
            resize(NONE, keysyms::KEY_k, Direction::Up, false),
            resize(NONE, keysyms::KEY_l, Direction::Right, false),
        ],
    },
    BindHint {
        context: HintContext::Resize,
        keys: "Shift",
        label: "x5",
        priority: 2,
        chords: &[
            resize(SHIFT, keysyms::KEY_Left, Direction::Left, true),
            resize(SHIFT, keysyms::KEY_Right, Direction::Right, true),
            resize(SHIFT, keysyms::KEY_Up, Direction::Up, true),
            resize(SHIFT, keysyms::KEY_Down, Direction::Down, true),
            resize(SHIFT, keysyms::KEY_h, Direction::Left, true),
            resize(SHIFT, keysyms::KEY_j, Direction::Down, true),
            resize(SHIFT, keysyms::KEY_k, Direction::Up, true),
            resize(SHIFT, keysyms::KEY_l, Direction::Right, true),
        ],
    },
    BindHint {
        context: HintContext::Resize,
        keys: "Tab",
        label: "next edge",
        priority: 3,
        chords: &[
            Chord {
                mods: NONE,
                sym: keysyms::KEY_Tab,
                action: HintAction::ResizeNextEdge(false),
            },
            Chord {
                mods: SHIFT,
                sym: keysyms::KEY_Tab,
                action: HintAction::ResizeNextEdge(true),
            },
        ],
    },
    BindHint {
        context: HintContext::Resize,
        keys: "Ctrl",
        label: "move",
        priority: 4,

        chords: &[
            Chord {
                mods: CTRL,
                sym: keysyms::KEY_Left,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL,
                sym: keysyms::KEY_Right,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL,
                sym: keysyms::KEY_Up,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL,
                sym: keysyms::KEY_Down,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL,
                sym: keysyms::KEY_h,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL,
                sym: keysyms::KEY_j,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL,
                sym: keysyms::KEY_k,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL,
                sym: keysyms::KEY_l,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL_SHIFT,
                sym: keysyms::KEY_Left,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL_SHIFT,
                sym: keysyms::KEY_Right,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL_SHIFT,
                sym: keysyms::KEY_Up,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL_SHIFT,
                sym: keysyms::KEY_Down,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL_SHIFT,
                sym: keysyms::KEY_h,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL_SHIFT,
                sym: keysyms::KEY_j,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL_SHIFT,
                sym: keysyms::KEY_k,
                action: HintAction::MoveFloating,
            },
            Chord {
                mods: CTRL_SHIFT,
                sym: keysyms::KEY_l,
                action: HintAction::MoveFloating,
            },
        ],
    },
    BindHint {
        context: HintContext::Resize,
        keys: "Esc",
        label: "done",
        priority: 5,
        chords: &[
            Chord {
                mods: NONE,
                sym: keysyms::KEY_Escape,
                action: HintAction::ResizeDone,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_Return,
                action: HintAction::ResizeDone,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_KP_Enter,
                action: HintAction::ResizeDone,
            },
        ],
    },
];

const WINDOW_HINTS: &[BindHint] = &[
    BindHint {
        context: HintContext::Windows,
        keys: "letters",
        label: "jump",
        priority: 1,
        chords: &[Chord {
            mods: NONE,
            sym: keysyms::KEY_a,
            action: HintAction::WindowLetter,
        }],
    },
    BindHint {
        context: HintContext::Windows,
        keys: "Esc",
        label: "cancels",
        priority: 2,
        chords: &[Chord {
            mods: NONE,
            sym: keysyms::KEY_Escape,
            action: HintAction::WindowCancel,
        }],
    },
];
const OVERVIEW_HINTS: &[BindHint] = &[
    BindHint {
        context: HintContext::Overview,
        keys: "arrows/HJKL",
        label: "select",
        priority: 1,
        chords: &[
            Chord {
                mods: NONE,
                sym: keysyms::KEY_Left,
                action: HintAction::OverviewKey,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_Right,
                action: HintAction::OverviewKey,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_Up,
                action: HintAction::OverviewKey,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_Down,
                action: HintAction::OverviewKey,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_h,
                action: HintAction::OverviewKey,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_j,
                action: HintAction::OverviewKey,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_k,
                action: HintAction::OverviewKey,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_l,
                action: HintAction::OverviewKey,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_Tab,
                action: HintAction::OverviewKey,
            },
            Chord {
                mods: SHIFT,
                sym: keysyms::KEY_Tab,
                action: HintAction::OverviewKey,
            },
        ],
    },
    BindHint {
        context: HintContext::Overview,
        keys: "Enter",
        label: "open",
        priority: 2,
        chords: &[Chord {
            mods: NONE,
            sym: keysyms::KEY_Return,
            action: HintAction::OverviewKey,
        }],
    },
    BindHint {
        context: HintContext::Overview,
        keys: "Delete",
        label: "close",
        priority: 4,
        chords: &[Chord {
            mods: NONE,
            sym: keysyms::KEY_Delete,
            action: HintAction::OverviewKey,
        }],
    },
    BindHint {
        context: HintContext::Overview,
        keys: "1-9",
        label: "workspace",
        priority: 3,
        chords: &[Chord {
            mods: NONE,
            sym: keysyms::KEY_1,
            action: HintAction::OverviewKey,
        }],
    },
    BindHint {
        context: HintContext::Overview,
        keys: "Esc",
        label: "close",
        priority: 5,
        chords: &[Chord {
            mods: NONE,
            sym: keysyms::KEY_Escape,
            action: HintAction::OverviewKey,
        }],
    },
];

const POPOVER_HINTS: &[BindHint] = &[
    BindHint {
        context: HintContext::Popover,
        keys: "↑↓ / JK",
        label: "select",
        priority: 1,
        chords: &[
            Chord {
                mods: NONE,
                sym: keysyms::KEY_j,
                action: HintAction::PopoverKey,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_k,
                action: HintAction::PopoverKey,
            },
        ],
    },
    BindHint {
        context: HintContext::Popover,
        keys: "←→ / HL",
        label: "adjust",
        priority: 2,
        chords: &[
            Chord {
                mods: NONE,
                sym: keysyms::KEY_h,
                action: HintAction::PopoverKey,
            },
            Chord {
                mods: NONE,
                sym: keysyms::KEY_l,
                action: HintAction::PopoverKey,
            },
        ],
    },
    BindHint {
        context: HintContext::Popover,
        keys: "Enter / Space",
        label: "activate",
        priority: 3,
        chords: &[Chord {
            mods: NONE,
            sym: keysyms::KEY_Return,
            action: HintAction::PopoverKey,
        }],
    },
    BindHint {
        context: HintContext::Popover,
        keys: "Esc",
        label: "close",
        priority: 1,
        chords: &[Chord {
            mods: NONE,
            sym: keysyms::KEY_Escape,
            action: HintAction::PopoverKey,
        }],
    },
];

pub fn items(context: HintContext) -> &'static [BindHint] {
    match context {
        HintContext::Code => CODE_HINTS,
        HintContext::Resize => RESIZE_HINTS,
        HintContext::Windows => WINDOW_HINTS,
        HintContext::Overview => OVERVIEW_HINTS,
        HintContext::Popover => POPOVER_HINTS,
    }
}

/// Keeps the most important hints that fit, preserving their display order.
/// `width` measures one hint including its label and key caps; `gap` separates
/// neighbours.
pub fn fit<'a>(
    hints: &[&'a BindHint],
    max_width: i32,
    gap: i32,
    width: impl Fn(&BindHint) -> i32,
) -> Vec<&'a BindHint> {
    let mut keep: Vec<_> = hints.iter().enumerate().collect();
    let total = |entries: &[(usize, &&BindHint)]| {
        entries.iter().map(|(_, h)| width(h).max(0)).sum::<i32>()
            + gap.max(0) * entries.len().saturating_sub(1) as i32
    };
    while total(&keep) > max_width.max(0) {
        let Some((drop, _)) = keep
            .iter()
            .enumerate()
            .max_by_key(|(_, (i, h))| (h.priority, *i))
        else {
            break;
        };
        keep.remove(drop);
    }
    keep.into_iter().map(|(_, hint)| *hint).collect()
}

/// Plain-text rendering used by tests and the cheat sheet. Code
/// hints carry the configured Mod name; resize-mode keys have no Mod.
#[cfg(test)]
pub fn plain_text(context: HintContext, mod_label: &str) -> String {
    items(context)
        .iter()
        .map(|hint| match context {
            HintContext::Code => format!("{} {} {}", hint.label, mod_label, hint.keys),
            _ => format!("{} {}", hint.keys, hint.label),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

pub(crate) fn modifiers(
    mod_key: crate::config::ModKey,
    mods: ChordMods,
) -> smithay::input::keyboard::ModifiersState {
    use crate::config::ModKey;
    let mut state = smithay::input::keyboard::ModifiersState::default();
    state.ctrl = mods.ctrl;
    state.shift = mods.shift;
    state.alt = mods.alt;
    match mod_key {
        ModKey::Super => {
            state.logo = mods.main;
            state.alt |= mods.extra;
        }
        ModKey::Alt => {
            state.alt |= mods.main;
            state.logo = mods.extra;
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::input::keyboard::Keysym;

    #[test]
    fn strip_text_lists_each_context_in_order() {
        let code = plain_text(HintContext::Code, "Super");
        for part in [
            "focus Super HJKL",
            "move Super Ctrl+HJKL",
            "resize Super Shift+HJKL",
            "hints Super G",
            "resize mode Super R",
            "overview Super A",
            "workspaces Super 1-9",
            "cheat sheet Super /",
        ] {
            assert!(code.contains(part), "{part} missing from {code}");
        }
        let resize = plain_text(HintContext::Resize, "Super");
        for part in [
            "arrows/HJKL edge",
            "Shift x5",
            "Tab next edge",
            "Ctrl move",
            "Esc done",
        ] {
            assert!(resize.contains(part), "{part} missing from {resize}");
        }
        assert!(!resize.contains("Super"), "resize-mode keys have no Mod");
    }

    #[test]
    fn lower_priority_hints_are_dropped_first() {
        let hints: Vec<&BindHint> = items(HintContext::Code).iter().collect();
        let width = |hint: &BindHint| match hint.priority {
            1 => 100,
            2 => 100,
            3 => 100,
            _ => 100,
        };
        let fitted = fit(&hints, 250, 10, width);
        assert_eq!(
            fitted.iter().map(|hint| hint.label).collect::<Vec<_>>(),
            ["focus", "move"]
        );
        let fitted = fit(&hints, 400, 10, width);
        assert_eq!(
            fitted.iter().map(|hint| hint.label).collect::<Vec<_>>(),
            ["focus", "move", "resize"]
        );
        // Empty space keeps everything, and the original order is preserved.
        let fitted = fit(&hints, 10_000, 10, width);
        assert_eq!(fitted.len(), hints.len());
        assert_eq!(fitted[4].label, "resize mode");
        assert!(fit(&hints, 50, 10, width).is_empty());
    }

    #[test]
    fn every_wired_code_chord_matches_the_dispatcher() {
        let mut h = crate::state::tests::Harness::new();
        // The TTY default: Mod is Super, so Ctrl+Mod+K is not Ctrl+Alt+K.
        h.gv.config.mod_key = crate::config::ModKey::Super;
        h.gv.set_code_mode(true);
        for hint in items(HintContext::Code) {
            for chord in hint.chords {
                let HintAction::Bind(expected) = &chord.action else {
                    continue;
                };
                let mods = modifiers(h.gv.config.mod_key, chord.mods);
                let actual = h.gv.bind_for(Keysym::from(chord.sym), &mods);
                assert_eq!(
                    actual,
                    Some(expected.clone()),
                    "{} / {}",
                    hint.label,
                    hint.keys
                );
            }
        }
    }

    #[test]
    fn code_aliases_replace_lock_and_hdr_only_while_enabled() {
        use crate::config::ModKey;
        let mut h = crate::state::tests::Harness::new();
        for mod_key in [ModKey::Super, ModKey::Alt] {
            h.gv.config.mod_key = mod_key;
            let main = modifiers(mod_key, MAIN);
            let ctrl_alt = smithay::input::keyboard::ModifiersState {
                ctrl: true,
                alt: true,
                ..Default::default()
            };
            h.gv.set_code_mode(false);
            assert_eq!(
                h.gv.bind_for(Keysym::from(keysyms::KEY_l), &main),
                Some(Action::Lock)
            );
            h.gv.set_code_mode(true);
            assert_eq!(
                h.gv.bind_for(Keysym::from(keysyms::KEY_l), &main),
                Some(Action::FocusDirection(Direction::Right))
            );
            assert_eq!(
                h.gv.bind_for(Keysym::from(keysyms::KEY_l), &ctrl_alt),
                Some(Action::Lock)
            );
            // Mod+Shift+H is HDR outside code mode and a resize step inside it.
            let shift = modifiers(mod_key, MAIN_SHIFT);
            h.gv.set_code_mode(false);
            assert_eq!(
                h.gv.bind_for(Keysym::from(keysyms::KEY_h), &shift),
                Some(Action::ToggleHdr)
            );
            h.gv.set_code_mode(true);
            assert_eq!(
                h.gv.bind_for(Keysym::from(keysyms::KEY_h), &shift),
                Some(Action::Resize(Direction::Left))
            );
        }
    }
}

/// Global chord inventory, next to the dispatcher. Labels are resolved from
/// the dispatched action, so Classic overrides and configured commands stay accurate.
#[derive(Clone, Copy)]
struct Shortcut {
    keys: &'static str,
    sym: u32,
    mods: ChordMods,
}
const EXTRA: ChordMods = ChordMods {
    extra: true,
    ..MAIN
};
const CTRL_ALT: ChordMods = ChordMods {
    ctrl: true,
    alt: true,
    ..NONE
};
const ALT: ChordMods = ChordMods { alt: true, ..NONE };
const CTRL_SHIFT: ChordMods = ChordMods {
    ctrl: true,
    shift: true,
    ..NONE
};
macro_rules! shortcuts {
    ($(($keys:literal, $sym:ident, $mods:ident)),* $(,)?) => {
        &[$(Shortcut { keys: $keys, sym: keysyms::$sym, mods: $mods }),*]
    };
}
const GLOBAL: &[Shortcut] = shortcuts![
    ("Return", KEY_Return, MAIN),
    ("KP Enter", KEY_KP_Enter, MAIN),
    ("D", KEY_d, MAIN),
    ("E", KEY_e, MAIN),
    ("Q", KEY_q, MAIN),
    ("M", KEY_m, MAIN),
    ("Ctrl+F", KEY_f, MAIN_CTRL),
    ("Shift+F", KEY_f, MAIN_SHIFT),
    ("F1", KEY_F1, MAIN),
    ("Left", KEY_Left, MAIN),
    ("Down", KEY_Down, MAIN),
    ("Up", KEY_Up, MAIN),
    ("Right", KEY_Right, MAIN),
    ("Ctrl+Left", KEY_Left, MAIN_CTRL),
    ("Ctrl+Down", KEY_Down, MAIN_CTRL),
    ("Ctrl+Up", KEY_Up, MAIN_CTRL),
    ("Ctrl+Right", KEY_Right, MAIN_CTRL),
    ("Shift+Left", KEY_Left, MAIN_SHIFT),
    ("Shift+Down", KEY_Down, MAIN_SHIFT),
    ("Shift+Up", KEY_Up, MAIN_SHIFT),
    ("Shift+Right", KEY_Right, MAIN_SHIFT),
    ("Extra+Left", KEY_Left, EXTRA),
    ("Extra+Down", KEY_Down, EXTRA),
    ("Extra+Up", KEY_Up, EXTRA),
    ("Extra+Right", KEY_Right, EXTRA),
    ("T", KEY_t, MAIN),
    ("Space", KEY_space, MAIN),
    ("Extra+Space", KEY_space, EXTRA),
    ("Shift+I", KEY_i, MAIN_SHIFT),
    ("Tab", KEY_Tab, MAIN),
    ("Shift+Tab", KEY_Tab, MAIN_SHIFT),
    ("Shift+1-9", KEY_1, MAIN_SHIFT),
    ("Ctrl+Page Up", KEY_Page_Up, MAIN_CTRL),
    ("Ctrl+Page Down", KEY_Page_Down, MAIN_CTRL),
    ("Shift+C", KEY_c, MAIN_SHIFT),
    ("Shift+M", KEY_m, MAIN_SHIFT),
    ("Shift+W", KEY_w, MAIN_SHIFT),
    ("L", KEY_l, MAIN),
    ("Shift+H", KEY_h, MAIN_SHIFT),
    ("Shift+F2", KEY_F2, MAIN_SHIFT),
    ("Shift+F3", KEY_F3, MAIN_SHIFT),
    ("Shift+S", KEY_s, MAIN_SHIFT),
    ("I", KEY_i, MAIN),
    ("N", KEY_n, MAIN),
    ("Alt+Tab", KEY_Tab, ALT),
    ("Ctrl+Alt+L", KEY_l, CTRL_ALT),
    ("Ctrl+Alt+Delete", KEY_Delete, CTRL_ALT),
    ("Ctrl+Alt+F1-12", KEY_XF86Switch_VT_1, CTRL_ALT),
    ("Print", KEY_Print, NONE),
    ("Shift+Print", KEY_Print, SHIFT),
    ("Alt+F4", KEY_F4, ALT),
    ("Ctrl+Shift+Esc", KEY_Escape, CTRL_SHIFT),
];

fn inventory() -> impl Iterator<Item = Shortcut> {
    GLOBAL.iter().copied().chain(
        items(HintContext::Code)
            .iter()
            .filter(|hint| !hint.keys.contains("HJKL"))
            .filter_map(|hint| {
                hint.chords.first().map(|chord| Shortcut {
                    keys: hint.keys,
                    sym: chord.sym,
                    mods: chord.mods,
                })
            }),
    )
}

fn action_label(action: &Action) -> (&'static str, &'static str) {
    use Action::*;
    match action {
        Spawn(_) | ToggleLauncher => ("Apps", "launch"),
        CloseWindow => ("Windows", "close"),
        Minimize => ("Windows", "minimize"),
        ToggleMaximize => ("Windows", "maximize"),
        ToggleFullscreen => ("Windows", "fullscreen"),
        WindowHints => ("Windows", "window hints"),
        CheatSheet => ("Windows", "cheat sheet"),
        PanelControls(crate::panel_controls::Kind::Wifi) => ("Panel", "Wi-Fi"),
        PanelControls(crate::panel_controls::Kind::Bluetooth) => ("Panel", "Bluetooth"),
        PanelControls(crate::panel_controls::Kind::Audio) => ("Panel", "volume mixer"),
        ToggleTiling => ("Tiling", "workspace tiling"),
        ToggleFloating => ("Tiling", "float window"),
        RotateSplit => ("Tiling", "rotate split"),
        FocusDirection(_) => ("Focus and swap", "focus"),
        Swap(_) => ("Focus and swap", "swap"),
        Snap(_) => ("Snap", "snap / move tile"),
        Resize(_) => ("Resize", "resize"),
        ResizeMode => ("Resize", "resize mode"),
        Workspace(_) => ("Workspaces", "switch workspace"),
        MoveWorkspace(_) => ("Workspaces", "send to workspace"),
        WorkspaceStep(_) => ("Workspaces", "cycle workspace"),
        ToggleOverview | CloseOverview => ("Overview", "overview"),
        SwitcherCycle(_) | SwitcherFinish(_) => ("Focus and swap", "window switcher"),
        ToggleCodeMode => ("Code mode", "toggle code mode"),
        ToggleFingerPointer => ("Display", "finger pointer (camera)"),
        Lock => ("Session", "lock"),
        Quit => ("Session", "quit compositor"),
        ToggleHdr => ("Display", "toggle HDR"),
        SdrWhite(_) => ("Display", "SDR white level"),
        SwitchVt(_) => ("Session", "switch VT"),
        Classic(action) => {
            use crate::classic::binds::ClassicAction::*;
            (
                "Classic",
                match action {
                    ToggleMode => "switch shell",
                    Start => "Start",
                    ShowDesktop => "show desktop",
                    MinimizeAll => "minimize all",
                    Settings => "settings",
                    TaskManager => "task manager",
                    Quick => "quick settings",
                    Notifications => "notifications",
                    Up => "maximize",
                    Down => "restore / minimize",
                    Item(_) => "taskbar app",
                    TaskView => "task view",
                    Close => "close / power menu",
                    Run => "run command",
                },
            )
        }
    }
}

pub fn cheat_rows(gv: &crate::Gavno) -> Vec<(String, String)> {
    use smithay::input::keyboard::Keysym;
    let mod_label = match gv.config.mod_key {
        crate::config::ModKey::Super => "Super",
        crate::config::ModKey::Alt => "Alt",
    };
    let extra = if mod_label == "Super" { "Alt" } else { "Super" };
    let mut groups: Vec<(&str, Vec<(String, String)>)> = Vec::new();
    for shortcut in inventory() {
        let mods = modifiers(gv.config.mod_key, shortcut.mods);
        let Some(action) = gv.bind_for(Keysym::from(shortcut.sym), &mods) else {
            continue;
        };
        let (group, mut label) = action_label(&action);
        if matches!(action, Action::Spawn(_)) {
            label = match shortcut.sym {
                keysyms::KEY_Return | keysyms::KEY_KP_Enter => "terminal",
                keysyms::KEY_e => "file manager",
                keysyms::KEY_d => "launcher",
                keysyms::KEY_Print | keysyms::KEY_s => "screenshot",
                _ => label,
            };
        }
        // Classic has no code-mode toggle.
        if gv.classic_mode() && action == Action::ToggleCodeMode {
            continue;
        }
        let keys = shortcut.keys.replace("Extra", extra);
        let keys = if shortcut.mods.main {
            format!("{mod_label}+{keys}")
        } else {
            keys
        };
        if let Some((_, rows)) = groups.iter_mut().find(|(g, _)| *g == group) {
            rows.push((keys, label.into()));
        } else {
            groups.push((group, vec![(keys, label.into())]));
        }
    }
    let mut rows = Vec::new();
    for (group, entries) in groups {
        rows.push((String::new(), group.into()));
        rows.extend(entries);
    }
    // The very same context entries feed the bar and the cheat sheet.
    for context in [
        HintContext::Code,
        HintContext::Resize,
        HintContext::Windows,
        HintContext::Overview,
        HintContext::Popover,
    ] {
        let active = match context {
            HintContext::Code => gv.code_mode,
            HintContext::Resize => gv.overlay.resize_mode.active(),
            HintContext::Windows => gv.overlay.keyboard.windows.is_some(),
            HintContext::Overview => gv.overview.open(),
            HintContext::Popover => gv.panel_controls_open(),
        };
        if !active {
            continue;
        }
        rows.push((String::new(), format!("{context:?} keys")));
        for hint in items(context) {
            if context == HintContext::Code && !hint.keys.contains("HJKL") {
                continue;
            }
            // Read each entry's dispatch metadata, rather than maintaining a second table.
            if hint.chords.is_empty() {
                continue;
            }
            let keys = if context == HintContext::Code {
                format!("{mod_label}+{}", hint.keys)
            } else {
                hint.keys.into()
            };
            rows.push((keys, hint.label.into()));
            if context == HintContext::Overview
                && hint.chords.iter().any(|c| c.sym == keysyms::KEY_Tab)
            {
                rows.push(("Tab / Shift+Tab".into(), "cycle selection".into()));
            }
        }
    }
    if gv.code_mode {
        if let Some(shortcut) = inventory().find(|s| s.mods.extra && s.sym == keysyms::KEY_Left) {
            let mods = modifiers(gv.config.mod_key, shortcut.mods);
            if let Some(action) = gv.bind_for(Keysym::from(keysyms::KEY_h), &mods) {
                rows.push((
                    format!("{mod_label}+{extra}+HJKL"),
                    action_label(&action).1.into(),
                ));
            }
        }
        if gv.config.mod_key == crate::config::ModKey::Alt {
            rows.push(("Ctrl+Alt+L".into(), "lock (emergency chord)".into()));
        }
    }
    rows
}

#[cfg(test)]
mod inventory_tests {
    use super::*;
    use smithay::input::keyboard::Keysym;

    #[test]
    fn every_global_chord_is_dispatched_in_its_shell() {
        let mut h = crate::state::tests::Harness::new();
        for mod_key in [crate::config::ModKey::Super, crate::config::ModKey::Alt] {
            h.gv.config.mod_key = mod_key;
            for shortcut in inventory() {
                let mods = modifiers(mod_key, shortcut.mods);
                h.gv.classic.mode = crate::classic::ShellMode::Gavno;
                let normal = h.gv.bind_for(Keysym::from(shortcut.sym), &mods);
                h.gv.classic.mode = crate::classic::ShellMode::Classic;
                let classic = h.gv.bind_for(Keysym::from(shortcut.sym), &mods);
                assert!(
                    normal.is_some() || classic.is_some(),
                    "unhandled {} ({mod_key:?})",
                    shortcut.keys
                );
            }
        }
    }

    #[test]
    fn strip_text_includes_window_and_overview_contexts() {
        assert_eq!(
            plain_text(HintContext::Windows, "Alt"),
            "letters jump · Esc cancels"
        );
        assert_eq!(
            plain_text(HintContext::Overview, "Alt"),
            "arrows/HJKL select · Enter open · Delete close · 1-9 workspace · Esc close"
        );
    }

    #[test]
    fn narrow_strip_drops_lower_priority_even_when_it_is_shorter() {
        let hints: Vec<_> = items(HintContext::Code).iter().collect();
        assert!(fit(&hints, 90, 10, |h| if h.priority == 1 { 100 } else { 10 }).is_empty());
    }

    #[test]
    fn cheat_sheet_follows_classic_overrides_and_code_aliases() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.config.mod_key = crate::config::ModKey::Super;
        let normal = cheat_rows(&h.gv);
        assert!(normal.contains(&("Super+L".into(), "lock".into())));
        h.gv.set_code_mode(true);
        let code = cheat_rows(&h.gv);
        assert!(code.contains(&("Super+HJKL".into(), "focus".into())));
        assert!(code.contains(&("Super+L".into(), "focus".into())));
        h.gv.set_code_mode(false);
        h.gv.classic.mode = crate::classic::ShellMode::Classic;
        let classic = cheat_rows(&h.gv);
        assert!(classic.contains(&("Super+A".into(), "quick settings".into())));
        assert!(!classic.iter().any(|(_, label)| label == "toggle code mode"));
    }
}

#[cfg(test)]
mod modal_inventory_tests {
    use super::*;
    #[test]
    fn every_context_chord_is_accepted_by_its_modal_dispatcher() {
        let mut h = crate::state::tests::Harness::new();
        let output = smithay::output::Output::new(
            "keyboard".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: (1600, 900).into(),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );
        h.gv.space.map_output(&output, (0, 0));
        let window = h.window();
        h.gv.map_new_window(window.clone());
        h.commit_size(&window, 400, 300);
        h.gv.focus_window(Some(&window));
        h.gv.set_code_mode(true);
        for hint in items(HintContext::Resize) {
            for chord in hint.chords {
                h.gv.enter_resize_mode();
                assert!(
                    h.gv.resize_mode_key_with_ctrl(chord.sym, chord.mods.shift, chord.mods.ctrl),
                    "{}",
                    hint.keys
                );
                h.gv.exit_resize_mode();
            }
        }
        for hint in items(HintContext::Windows) {
            for chord in hint.chords {
                h.gv.toggle_window_hints();
                h.gv.keyboard_overlay_key(chord.sym);
                assert!(!h.gv.overlay.keyboard.open());
            }
        }
        for hint in items(HintContext::Overview) {
            for chord in hint.chords {
                h.gv.toggle_overview();
                assert!(h.gv.overview.open());
                h.gv.overview_key(chord.sym, chord.mods.shift);
                h.gv.close_overview();
            }
        }
    }
}
