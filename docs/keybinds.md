# Keyboard binds

These are the bindings implemented in `crates/gavno/src/input.rs`. "Mod" is the
configurable mod key: Super on the TTY backend, Alt when nested (`--winit`).
`GAVNO_MOD=super` or `GAVNO_MOD=alt` overrides the default.

Binds match the Latin layout, so they also work while the Russian layout is
active.

## Apps

| Keys | Action |
|---|---|
| Mod+Return, Mod+KP Enter | Start the terminal from `GAVNO_TERMINAL` (default `foot`). |
| Mod+D | Start the launcher from `GAVNO_LAUNCHER`. Unset by default; without it the key goes to the client. |

## Windows

| Keys | Action |
|---|---|
| Mod+Q | Close the focused window. |
| Mod+Ctrl+F | Toggle maximize. |
| Mod+Shift+F | Toggle fullscreen. |

## Snap

| Keys | Action |
|---|---|
| Mod+Ctrl+Left | Snap to the left half. If already snapped left, move to the previous output. |
| Mod+Ctrl+Right | Snap to the right half. If already snapped right, move to the next output. |
| Mod+Ctrl+Up | From a side half, snap to the top quarter on that side; otherwise maximize. |
| Mod+Ctrl+Down | Step down: maximized goes to floating, a top quarter to the side half, a side half to the bottom quarter. |

Left and Right also move between quarters of a row (for example TopRight +
Ctrl+Left gives TopLeft).

## Focus and swap

| Keys | Action |
|---|---|
| Mod+Left, Mod+Right, Mod+Up, Mod+Down | Focus the nearest window in that direction. |
| Mod+Alt+Left/Right/Up/Down | Swap with the nearest window in that direction, keeping both geometry slots. |
| Mod+Super+Left/Right/Up/Down | Same swap when Mod is Alt (nested default), because Alt is then taken by Mod. |

## Resize

| Keys | Action |
|---|---|
| Mod+Shift+Left, Mod+Shift+Up | Shrink the focused floating window by 40 px per press. |
| Mod+Shift+Right, Mod+Shift+Down | Grow the focused floating window by 40 px per press. |

Resize only applies to floating windows; snapped, maximized and fullscreen
windows ignore it.

## Alt+Tab switcher

| Keys | Action |
|---|---|
| Alt+Tab | Open the switcher and cycle forward. |
| Alt+Shift+Tab | Open the switcher and cycle backward. |
| Tab, Shift+Tab | Cycle forward or backward while the switcher is open. |
| Escape | Close the switcher without switching. |
| Release Alt | Focus the selected window. |

The switcher is a text list of windows in most-recently-used order.

## Session / VT

| Keys | Action |
|---|---|
| Ctrl+Alt+Delete | Quit GAVNO. |
| Ctrl+Alt+F1 ... F12 | Switch to virtual terminal 1-12 (TTY backend only). |

Ctrl+Alt+Delete and VT switching work even when the session is locked or a
client inhibits keyboard shortcuts. In `--greeter` mode every key except VT
switching goes to the login screen.
