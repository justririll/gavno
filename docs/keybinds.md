# Keyboard binds

These are the bindings implemented in `crates/gavno/src/input.rs`. "Mod" is the
configurable mod key: Super by default on the TTY backend, Alt when nested
(`--winit`, because the host compositor keeps Super for itself). Set `mod_key`
in [the config file](config.md) to change the default; `GAVNO_MOD=super` or
`GAVNO_MOD=alt` overrides both the default and the file.

Binds match the Latin layout, so they also work while the Russian layout is
active.

## Apps

| Keys | Action |
|---|---|
| Mod+Return, Mod+KP Enter | Start the terminal from `terminal` (default `foot`; `GAVNO_TERMINAL` overrides). |
| Mod+D | Start the launcher from `launcher`. Unset by default, so the key goes to the client; `GAVNO_LAUNCHER` overrides. |

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

## Workspaces

Each output has its own workspace list. Shortcuts count workspaces from 1, IPC
counts them from 0.

| Keys | Action |
|---|---|
| Mod+1 ... Mod+9 | Select workspace 1-9 on the active output. |
| Mod+Shift+1 ... Mod+Shift+9 | Move the focused window to workspace 1-9. |
| Mod+Tab, Mod+Ctrl+Page Down | Select the next workspace. |
| Mod+Shift+Tab, Mod+Ctrl+Page Up | Select the previous workspace. |

The number keys select an existing workspace, including the trailing empty one.
Selecting or moving to an index that does not exist does nothing.

The list is dynamic: using the trailing empty workspace appends another empty
one, and empty inactive workspaces are removed once the switch settles. Windows
on inactive workspaces keep their geometry, mode and place in their workspace's
focus order; they receive no input and are only drawn as read-only content in
the overview and in the switching strip.

With `mod_key = "alt"`, Mod+Tab is the workspace bind, so Alt+Tab does not open
the switcher.

## Overview

The overview zooms out the active output: all workspaces are shown side by side,
windows keep live content and their titles, and a strip of workspace thumbnails
sits at the top.

| Keys | Action |
|---|---|
| Mod+A | Toggle the overview. |
| Super tap | Toggle the overview: press and release Super with no other key held and no click in between. |
| Super+Shift+F23 | Toggle the overview. This is what the Copilot key sends on Snapdragon X Elite laptops. |
| Escape | Close the overview. |

| Pointer | Action |
|---|---|
| Left click a window | Focus it, switching to its workspace if needed, and close the overview. |
| Drag a window onto a workspace thumbnail | Move the window to that workspace. Moving less than 6 px counts as a click. |
| Left click a workspace thumbnail | Select that workspace. |
| Middle click a window | Ask the window to close. |

Other keys and pointer scroll are ignored while the overview is open.

## Touchpad gestures

| Gesture | Action |
|---|---|
| 3-finger swipe left/right | Drive the workspace strip. |
| 3-finger swipe up/down | Open or close the overview. |
| 3-finger hold | Catch a running workspace switch or overview animation; releasing settles to the nearest state. |
| 2-finger swipe, pinch or hold | Forwarded to the client under the pointer through pointer-gestures. |
| 2-finger swipe with Super held at the start | Behaves like a 3-finger swipe: horizontal drives the workspace strip, vertical the overview. |

A compositor-handled swipe locks to its dominant axis after 12 logical input
units. Horizontal movement rubber-bands past the first and last workspace. On
release the finger velocity is projected 0.18 seconds ahead and the nearest
workspace or overview state is selected; a cancelled swipe returns to where it
started. A 3-finger hold only catches an animation that is already running.

Three- and four-finger pinches and four-finger swipes are not handled yet.
Gestures are off in `--greeter` mode and while the session is locked.

`GAVNO_GESTURE_DISTANCE` sets the logical input units per workspace (default
300; values below 24 are ignored). The overview uses 340 units for its full
travel. `GAVNO_NATURAL_SCROLL=0` reverses the gesture direction and turns
natural scrolling off; natural scrolling is on by default.

## Session / VT

| Keys | Action |
|---|---|
| Ctrl+Alt+Delete | Quit GAVNO. |
| Ctrl+Alt+F1 ... F12 | Switch to virtual terminal 1-12 (TTY backend only). |

Ctrl+Alt+Delete and VT switching work even when the session is locked or a
client inhibits keyboard shortcuts. In `--greeter` mode every key except VT
switching goes to the login screen.

## HDR output (DRM backend)

- Mod+Shift+H: toggle HDR on the active output, when EDID and DRM support it.
- Mod+Shift+F2 / Mod+Shift+F3: decrease / increase SDR white by 10 nits while
  HDR is enabled (80–400 nits, default 203).

See [configuration](config.md) for mode selection, HDR requirements, and IPC.
